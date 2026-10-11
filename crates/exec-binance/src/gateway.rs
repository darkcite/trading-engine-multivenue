// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The gateway thread (plan §3.3).** The single writer of every Binance
//! socket and every Binance table.
//!
//! **Lanes (BX7, M3).** Each armed futures product has its own LANE — its
//! WS API order sessions (or its REST order route), its user stream and
//! listenKey, its REST jobs connection and queue, its margin state —
//! USDⓈ-M's on fapi, COIN-M's on dapi — and so does spot (BX8): its WS API
//! sessions carry the user data too (each logs on, then subscribes; only
//! the subscription makes it live), its REST jobs are `api/v3`'s, it has
//! no listenKey, no margin, no dead-man and no maker — and so do options
//! (BX9: eapi's, below). Everything else is ONE: the
//! open-order table and the rows, the held queue and fill lane 4, the
//! journal and the stats block, the signer (options excepted: their own),
//! the clock (Binance's one server time, measured on the first lane), the
//! desk (ids and deadlines) and the weight window (one IP pool; eapi's is
//! its own). A verb goes to its row's lane; a
//! user event is its own lane's; a REST job carries its lane. The pulse
//! is fail-closed across lanes: up only while every lane is, its gap the
//! longest.
//!
//! ```text
//! loop {
//!   0. the halt's sweep request (the stats word, XG-1)
//!   1. latency path first: every queued command rendered into ONE burst
//!      on its lane's active order session, then ONE flush (the E7 lesson) — no
//!      command taken while events are held or the event ring is short of
//!      room (XG-2)
//!   2. sockets: order answers (ACK / refusal) · user events (fills, states,
//!      margin) · REST answers (listenKey, dead-man, recon, the day) — the
//!      poller parks only when quiet, never past a deadline (the kit's Idle)
//!   3. the deadline queue (TTL and owed cancels, each WS API request's
//!      deadline) and the coarse timers (every ~1 ms): the pulse (100 ms) ·
//!      the dead-man heartbeat · the clock (60 s) · recon + margin · the
//!      listenKey keep-alive (30 min) · in-doubt queries · the order
//!      sessions' make-before-break · the 23 h rotation
//! }
//! ```
//!
//! **The kit (BX7, O-XH13).** The gateway runs on `exec-gateway`: the
//! engine/gateway duplex and its stats block (the halt and sweep words,
//! the heartbeat, the published tallies, the latencies), the in-flight
//! table ([`Pending`], the WS API ids), one deadline queue
//! ([`TimerQueue`]), the make-before-break machine ([`Mbb`]) over TWO
//! order sessions, and the wait policy ([`Idle`]). A planned replacement —
//! the 23 h rotation, the venue's `serverShutdown` — dials and logs on the
//! new session while the old one carries posts; the old one then drains
//! (its answers still land) and is closed when empty. A session judged
//! dead (S2: a request unanswered for [`WS_REQ_TIMEOUT_NS`]; a failed
//! socket) is dropped at once: its requests are in doubt, and nothing is
//! sent until a new one logs on.
//!
//! **Boot** ([`BnGateway::boot`]) runs the same loop on the BOOT thread
//! until the gateway is ready: the clock measured, the BX-19 assertions
//! held (per lane one-way mode — and single-asset margin on USDⓈ-M; once,
//! a key that trades futures and cannot withdraw, and the account mode;
//! per owned USDⓈ-M row cross margin and a leverage at most
//! `max_leverage` — COIN-M's rows by their first account read, PM's by
//! construction),
//! every lane's listenKey created, order route up and user stream open,
//! the first reconciliation run with every orphan swept (BX-9) and the
//! day's spend read (obligation 8).
//! An assertion that fails refuses the boot. Only then is the gateway
//! moved onto its own thread ([`BnGateway::run`]).
//!
//! **Demo (BX7b, the battery's).** Binance Demo serves no SAPI — every
//! `/sapi/…` path answers 404 — and offers no Portfolio Margin. A Demo
//! account is therefore classic by construction: under `Scope::Demo` the
//! gateway refuses any other account mode at construction, and its boot
//! makes neither SAPI read (the key's permissions, the account mode). The
//! scope is the configuration's, and a configuration holds only its own
//! network's hosts (`BnConfig`, BX-16): the engine's arm (`Scope::Live`)
//! makes both reads on every boot, and refuses when either cannot be read.
//!
//! **A restart with a position (BX7, the operator's ruling).** At the
//! boot's first reconciliation, on a DEDICATED account, the venue's
//! positions on owned rows are adopted as the owner slot's starting
//! inventory if their exposure in all (linear at the read's mark, inverse
//! at the face) fits the slot's instance cap — booked, pushed on lane 4 as
//! fills flagged `FILL_FLAG_ADOPTED` (the router's ledger books them as
//! inventory: no turnover, no resting order) and journaled; past the cap
//! — or past what lane 4 and the held queue can take, checked before
//! anything is booked — the boot is refused ([`BootErr::Inventory`]). The
//! day's report — and so the arm's `reconciled`, which seeds the slot —
//! waits until lane 4 has delivered them. Never adopted (fail closed: the
//! leg stays unseen, the slot unreconciled, until it is flattened by hand):
//! a row that is not live (the arm could not exit it), and anything on a
//! shared account. A leg this boot already booked is not adopted again —
//! any difference is the reconciliation's (drift).
//!
//! **Portfolio Margin (BX7, C2; `account_mode = "pm"`).** Every lane is
//! papi's: REST only (papi has no WS API: the order route is the REST
//! route, the WS pair is never dialled), the product's own papi paths,
//! IoC-only (K13: no dead-man). The account has ONE user stream
//! (`/pm/ws/<listenKey>`, `POST /papi/v1/listenKey`), held by the first
//! lane; each event names its product (`fs`). A lane's account step reads
//! its positions alone; the margin, the session equity and the account's
//! status come from the unified account (`/papi/v1/account`) — the cycle's
//! last step, before its verdict: a cycle that cannot read and scan it
//! fails (its staleness halts as any other's), and the boot needs one — and
//! on re-reads. `riskLevelChange`: a margin call is BX-20's; a
//! reduce-only or liquidating account is a `VenueLock`. papi has no time
//! endpoint: the clock reads the SAPI host's, its connection kept.
//!
//! **PM Pro (BX7, C3; `account_mode = "pm_pro"`).** The classic lanes and
//! routes (fills on the classic streams), IoC-only until K14 shows the
//! dead-man there; no single-asset assertion (the program's collateral).
//! The margin, the equity and the status are the unified account's, on
//! SAPI (`/sapi/v1/portfolio/account`, its connection kept); the USDⓈ-M
//! lane also holds the risk stream (`/pm-classic/ws/<its key>`):
//! `riskLevelChange` as PM's, `NORMAL` changing nothing. Risk guidance
//! only — its outage gates nothing: the account's status is read in every
//! cycle, which fails without it (as Portfolio Margin's).
//!
//! **Reconciliation.** A cycle reads each lane's account, then its open
//! orders, in lane order, and judges every row's leg ONCE after the last
//! lane: an inverse (COIN-M) leg's difference is valued at the contract's
//! face. The margin is per product (COIN-M's: the riskiest asset's
//! ratio); the session equity (E7) is USDⓈ-M's. A lane that cannot read
//! fails the cycle — no verdict without every lane (nor, in Portfolio
//! Margin and PM Pro, without the unified account) — and so does a read
//! listing a bound row twice (the later entry could mask the earlier). The
//! options lane (BX9) reads its positions as a step of their own, between
//! its account and its open orders.
//!
//! **The account's configuration mid-session (BX7, BX-19).** A bound row's
//! leverage changed (`ACCOUNT_CONFIG_UPDATE`), the multi-assets mode turned
//! on where single-asset was asserted, an owned row's position shown
//! isolated (`ACCOUNT_UPDATE`'s `mt`, or no `mt` on a classic stream; no
//! event names a margin type change), or a COIN-M read showing an owned
//! row isolated, above the leverage cap or — a perpetual — no longer
//! listed: a `VenueLock` (once). The restart re-asserts.
//!
//! **DNS (BX7, carried).** Every connection resolves its host once, when
//! built. A host whose sockets fail [`crate::dns::RERESOLVE_AFTER`]
//! establishments in a row — and every as many more — is resolved again on
//! a cold thread ([`crate::dns`]: `getaddrinfo` blocks, so never here) and
//! the answer handed to every connection on it, for its next dial.
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
//! retried from the deadline queue. A `-2011` (no such open order) puts the
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
//! second round on (S4). On the REST order route it first retires every
//! place and modify still queued, unsent — the WS API would have sent them
//! at admission; here they would leave ahead of its cancels (BX7).
//!
//! **Quiet (BX-12).** After a budget or lock answer the gateway sends
//! NOTHING for [`QUIET_NS`] — or until the ban the venue names ends, up to
//! [`QUIET_MAX_NS`] (S6): no order, cancel, heartbeat, REST call or
//! reconnect (the dead-man then cancels the makers).
//!
//! Built for **USDⓈ-M × classic** (the only maker product, BX-17 after
//! §13.5), **COIN-M × classic** (BX7: IoC-only, O-BX3a — no dead-man of
//! ours there, so no maker, cancel or modify is built for it), **both ×
//! Portfolio Margin and × PM Pro** (BX7: IoC-only), and **spot × classic**
//! (BX8: IoC-only — spot has no dead-man, so no maker, cancel or modify is
//! built for it; its balances are reconciled at the venue's 8 decimals,
//! every armed row's permission groups judged at every account read), and
//! **options × classic** (BX9: IoC-only, below); stocks arrive with BX10.
//!
//! **Options (BX9, O-BX10).** eapi has no WS API: the lane's order route is
//! the REST route, always (its WS pair is never dialled), every request
//! signed by the options lane's OWN signer — the scope's HMAC key (Q-1:
//! eapi refuses an Ed25519 key) — under that key's header; the user stream
//! is `/private/ws/<listenKey>` on the options stream host. IoC-only: no
//! dead-man (the countdown is a market maker's, `-6005`), so no maker,
//! cancel or modify is built; a stray of ours refuses the boot by name
//! ([`BootErr::OptStray`]). The cycle reads the options account
//! (`marginAccount`, on the route: the ratio `maintMargin / adjustedEquity`,
//! the equity, `canTrade` and `reduceOnly` — a refusal before Ready, a
//! `VenueLock` after), then the positions (`/eapi/v1/position`: the legs,
//! valued at the mark), then the open orders. **Expiry:** a leg past its
//! row's expiry is SETTLING — not judged, the cycle still conclusive (its
//! exposure stays booked; the row trades no more) — until `exerciseRecord`
//! names it — the record books a settlement fill at the intrinsic value
//! per contract that flattens the row (`FILL_FLAG_SETTLEMENT`) — or
//! [`EXPIRY_HOLD_MAX_MS`] passes, when it is judged (fail closed: drift). A place on an expired
//! row is refused locally ([`CODE_EXPIRED`]). **Adoption:** a held long at
//! its mark; a held short at its initial margin's upper bound (mark +
//! 15 % of the index per contract, the index read once at boot,
//! `/eapi/v1/index`; unread, the boot is refused), and a held short on a
//! row the arm may not write refuses the boot ([`BootErr::OptionShort`]).
//! The session equity (E7, Q-4) is USDⓈ-M's margin balance plus the
//! options account's equity, 0 (unread) until each armed one is read.
//! eapi's REQUEST_WEIGHT is its own pool, metered in its own window: the
//! day read (`userTrades`, a row at a time) waits while that window cannot
//! take it beside a cycle (Q-5). A commission on the options stream is a
//! charge whatever its sign (IoC: always a taker's).
//!
//! **Spot's balance law (BX8).** A spot row's position IS its base asset's
//! balance, and a BUY's commission is charged in that asset: each spot row
//! keeps the balance its bookings explain, ×1e8 — the adopted balance, ±
//! every fill, − every commission charged in the asset (BNB's on any row,
//! when BNB is an armed base) — and the reconciliation compares it, never
//! the gross position the router books. One armed row per base asset, and
//! no armed base another row's quote: the asset is the row's.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clob_dispatcher::{
    RETIRED_CANCELED_MEMBER, RETIRED_CANCELED_TTL, RETIRED_CANCELED_VENUE, RETIRED_EXPIRED,
    RETIRED_FILLED, RETIRED_REJECTED,
};
use core_fill::ORDER_KIND_MAKER;
use core_net::{Backoff, ConnCfg, HttpsConn, Progress, WsCfg, WsConn, WsNext, WsProgress};
use core_ring::Producer;
use core_types::{ExecRecord, Fill, Price, Qty, Side, FILL_FLAG_ADOPTED, FILL_FLAG_SETTLEMENT, FILL_ORIGIN_VENUE};
use exec_gateway::stats::{
    CONGESTION_NONE, CONGESTION_RATE_LIMITED, CONGESTION_THROTTLED, HALT_TIMER_FULL, HALT_VENUE_BASE, SWEEP_CLEAR, SWEEP_STRANDED,
    SWEEP_WORKING,
};
use exec_gateway::{CmdRx, EvtTx, Expired, GatewayStats, Idle, Mbb, MbbAction, Pending, RateWindow, SlotState, TimerId, TimerQueue};
use mio::{Events, Poll, Token};

use crate::cid::{CidClass, CidPrefix};
use crate::clock::VenueClock;
use crate::cmd::*;
use crate::config::{BnConfig, Scope, Signer, Signing};
use crate::dns::{Resolver, DNS_HOSTS};
use crate::gov::{
    classify, request_weight, CodeClass, Weighed, ACCOUNT_REREAD_MS, ARM_WEIGHT_MAX_PER_MIN, OPT_ARM_WEIGHT_MAX_PER_MIN, QTR_ICR_NS, W_OPT_RECON,
    WEIGHT_THROTTLE_PCT,
};
use crate::inst::{
    AssetName, WireTable, INST_DATED, INST_INVERSE, INST_LIVE, INST_NO_DEADMAN, INST_OWNED, INST_WRITABLE, OPT_ULY_MAX, PRODUCT_COINM, PRODUCT_OPTIONS,
    PRODUCT_SPOT, PRODUCT_USDM, ROW_NONE, OptWire,
};
use crate::journal::{
    fee_asset, fee_code, fee_notional_1e9, JournalTx, FEE_ASSET_NONE, JOURNAL_RING, J_ACK, J_ANCHOR, J_ENDED, J_FEE, J_FILL, J_FILL_ADOPTED,
    J_FILL_LOST, J_FILL_UNSEEN, J_MARGIN, J_RECON, J_REJECT, J_SWEEP, J_VENUE, TRADE_BY_ADL, TRADE_BY_LIQUIDATION, TRADE_BY_MEMBER,
    TRADE_BY_ORPHAN, TRADE_BY_SETTLEMENT,
};
use crate::margin::um_ratio_1e6;
use crate::mode::{judge_key, judge_mode, judge_one_way, judge_permission, judge_single_asset, judge_symbol_config, AccountMode, AssertErr};
use crate::oot::{
    EndedRing, Oot, OotErr, TidRing, OF_CANCEL_SENT, OF_FILLED_ANY, OF_LISTED, OF_MODIFY_SENT, OF_STATUS_BOOKED, OF_TIMED,
    OF_TTL, OOT_CAP, OWE_MEMBER, OWE_TTL, OWE_UNKNOWN, ST_IN_DOUBT, ST_LIVE, ST_SENT,
};
use crate::recon::{
    day_buys_1e6, day_increasing_1e6, judge_positions, scan_cm_account, scan_exercise, scan_opt_margin, scan_opt_positions, scan_pm_account, scan_positions,
    scan_spot_account, scan_um_account, settlement_agrees, ExerciseRow, Leg, PosRow, Verdict, MARGIN_ISOLATED, PM_LOCKED, PM_MARGIN_CALL, SPOT_ASSETS_MAX, TRADES_PAGE,
};
use ingress_binance::discovery::BnPermissions;
use crate::rest::*;
use crate::userstream::*;
use crate::wsapi::*;

/// A lane's tokens are `LANE_TOKENS · lane + k`: its order sessions'
/// two slots (make-before-break), its user stream, its REST jobs, its REST
/// order route (BX7, C5).
const LANE_TOKENS: usize = 8;
const TK_WS0: usize = 0;
const TK_WS1: usize = 1;
const TK_US: usize = 2;
const TK_REST: usize = 3;
const TK_ORD: usize = 4;
/// PM Pro's risk stream (`/pm-classic/ws/<listenKey>`, BX7 C3).
const TK_RISK: usize = 5;
/// The boot-only SAPI connection (outside every lane's tokens).
const TOKEN_SAPI: Token = Token(LANE_TOKENS - 1);
const _: () = assert!(TK_ORD < LANE_TOKENS - 1 && TK_RISK < LANE_TOKENS - 1);
/// The lanes one gateway serves (BX7, M3: USDⓈ-M and COIN-M; BX8: spot;
/// BX9: options).
pub const LANES: usize = 4;
/// The lanes' order, by product.
const LANE_ORDER: [u8; LANES] = [PRODUCT_USDM, PRODUCT_COINM, PRODUCT_SPOT, PRODUCT_OPTIONS];

/// Every WS connection's buffers and clocks.
const WS_CFG: WsCfg = WsCfg {
    rx_cap: 256 * 1_024,
    tx_cap: 64 * 1_024,
    establish_ns: 10 * S,
    idle_ns: 240 * S,
};
/// Every REST connection's windows and timeout.
const CONN_CFG: ConnCfg = ConnCfg {
    win_cap: 4 * 1_024,
    resp_cap: 1_024 * 1_024,
    req_timeout_ns: 5 * S,
};

/// Lane `l`'s token `k`.
#[inline(always)]
const fn lane_token(l: usize, k: usize) -> Token {
    Token(LANE_TOKENS * l + k)
}

/// Lane `l`'s order session `s` as one index over every lane's two slots
/// (the in-flight table's `conn`, the desk's counts).
#[inline(always)]
const fn sess(l: usize, s: usize) -> usize {
    2 * l + s
}

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
/// WS API requests in flight at once, both order sessions together (the
/// in-flight table's size).
pub const WS_IDS: usize = 256;
/// An unanswered WS API request is judged after this — and the order
/// session with it: a request unanswered this long means the session is
/// dead whatever its socket says (S2).
pub const WS_REQ_TIMEOUT_NS: u64 = 5 * S;
/// XG-4: an order session's backoff resets once it has been live this long
/// and moved data (a matched answer).
pub const WS_HEALTHY_NS: u64 = 30 * S;
/// A dial's budget: the socket's establishment (10 s) and the logon's
/// deadline (5 s). A dial over it is given up (in doubt, backoff).
pub const WS_DIAL_BUDGET_NS: u64 = 15 * S;
/// A replaced session's drain budget: every request it carries is
/// answered or judged lost by then.
pub const WS_DRAIN_BUDGET_NS: u64 = WS_REQ_TIMEOUT_NS + S;
/// The deadline queue's capacity: a TTL or owed-cancel deadline per open
/// order and a deadline per WS API request — it cannot fill (XG-3: a full
/// queue is a sizing bug that halts).
pub const TIMER_CAP: usize = 2_048;
const _: () = assert!(TIMER_CAP >= OOT_CAP + WS_IDS);
/// The deadline kinds: an open order's TTL or owed cancel (key: the
/// table slot and its generation)…
const TK_ORDER: u8 = 1;
/// …and a WS API request's answer (key: the request id).
const TK_WS_REQ: u8 = 2;
/// XG-2: no command is taken while the event ring has less room than
/// this (four bursts' worth: a burst's local refusals and the answers
/// already on the wire) — the events wait in the held queue, the commands
/// in their ring (whose full is the arm's `QueueFull`).
pub const EVT_ROOM_MIN: usize = 4 * CMD_BURST;
/// The REST order route's queue: per open order at most a place, a modify,
/// a cancel and a status at once, and the account read.
const REST_Q: usize = 4 * OOT_CAP + 16;
/// The halt word's Binance reasons (XG-5: venue reasons from
/// `HALT_VENUE_BASE`): an event or fill neither its ring nor the held
/// queue could take — the arm no longer knows what rests.
pub const HALT_OUTPUT_LOST: u64 = HALT_VENUE_BASE;
/// The stats block's published word for the request weight spent in the
/// last minute (after the tallies' words).
pub const PW_WEIGHT: usize = crate::arm::GW_TALLIES;
/// BX9: the stats block's word carrying eapi's weight spent in the last
/// minute (options' own pool).
pub const PW_OPT_WEIGHT: usize = crate::arm::GW_TALLIES + 1;
const _: () = assert!(PW_OPT_WEIGHT < exec_gateway::stats::PUBLISHED_WORDS);
/// The pulse.
pub const STATUS_EVERY_NS: u64 = 100 * MS;
/// The clock round.
pub const CLOCK_EVERY_NS: u64 = 60 * S;
/// The 23 h rotation (connections live at most 24 h): the production
/// [`GwKnobs::rotate_after_ns`].
pub const ROTATE_AFTER_NS: u64 = 23 * 3_600 * S;
/// An in-doubt order is queried this often.
pub const IN_DOUBT_EVERY_NS: u64 = S;
/// Rounds of cancels a sweep tries before it reports `Stranded`.
pub const SWEEP_ROUNDS: u8 = 3;
/// Events and fills held while a ring is full.
pub const HELD_CAP: usize = 2_048;
/// Commands drained per pass.
pub const CMD_BURST: usize = 64;
/// A product with no lane.
const NO_LANE: u8 = u8::MAX;
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
/// The deadline queue is full (XG-3: a sizing bug; the halt word is set).
pub const CODE_TIMER_FULL: i32 = 9_009;
/// Portfolio Margin's `riskLevelChange` named a reduce-only or liquidating
/// account (or a level this build does not know): a `VenueLock` (BX7, C2).
pub const CODE_PM_RISK: i32 = 9_010;
/// Portfolio Margin's unified account reads a reduce-only, liquidating or
/// bankrupt status (or one this build does not know): a `VenueLock`.
pub const CODE_PM_ACCOUNT: i32 = 9_011;
/// `ACCOUNT_CONFIG_UPDATE` changed a bound row's leverage, or turned the
/// multi-assets mode on (BX-19's single-asset assertion no longer holds):
/// a `VenueLock` (BX7, carried).
pub const CODE_CONFIG: i32 = 9_012;
/// An owned row's position is not cross-margined mid-session (BX7, BX-19:
/// `ACCOUNT_UPDATE`'s `mt`, or COIN-M's account read): a `VenueLock`.
pub const CODE_MARGIN_TYPE: i32 = 9_013;
/// A place or modify the REST order route still held, unsent, when a sweep
/// began: retired, never sent (BX7) — a place refused, a modify aborted.
pub const CODE_SWEPT: i32 = 9_014;
/// The spot account can no longer trade an armed row mid-session — a
/// permission group the row needs is gone (the bStock check), or the
/// account answers `canTrade: false` (BX8, BX-19): a `VenueLock`.
pub const CODE_PERMISSION: i32 = 9_015;
/// BX9: the options account turned reduce-only or unable to trade
/// mid-session (`marginAccount`'s `reduceOnly` / `canTrade`, or the stream's
/// `RISK_LEVEL_CHANGE`): a `VenueLock`.
pub const CODE_OPT_ACCOUNT: i32 = 9_016;
/// BX9: a place on an option row past its expiry — refused locally (the
/// venue would refuse it; the leg waits for its settlement).
pub const CODE_EXPIRED: i32 = 9_017;
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
/// BX9: an expired option's leg is left unjudged (settling) at most this
/// long past its expiry (Binance settles at 08:00Z within the half hour):
/// then it is judged — a leg still booked reads as drift (fail closed).
pub const EXPIRY_HOLD_MAX_MS: u64 = 2 * 3_600_000;
/// BX9: the exercise records one read takes (`limit`; the venue's own
/// maximum is 1 000) — the account's settlements since the earliest
/// settling expiry, at most one a row it held: [`crate::inst::INST_MAX`]
/// rows bound, so one page holds every one of ours unless a shared
/// account settled more than this of its own in that window. Then the
/// records past the page are never read (each cycle asks from the same
/// expiry): their legs stay settling and are judged at
/// [`EXPIRY_HOLD_MAX_MS`] — drift, fail closed (the risk review's R11).
const EXERCISE_PAGE: usize = 256;
/// BX9: a short option's initial margin is at most the mark plus this
/// share of the index per contract (Binance's formula, D8: the rate of
/// `max(10 %·I, 15 %·I − OTM)` is at most 15 %) — the exposure a held
/// short is adopted at, ×1e6.
const OPT_SHORT_IM_MAX_1E6: i64 = 150_000;

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
    /// Busy-poll (the poller never parks) or the kit's default wait (park
    /// up to 100 µs once quiet, never past a deadline).
    pub spin: bool,
    /// The order session's planned replacement, make-before-break:
    /// [`ROTATE_AFTER_NS`] (the boot passes the constant; a test, less).
    pub rotate_after_ns: u64,
    /// The REST order route (`order_transport = "rest"`, BX7 C5) in place
    /// of the WS API: the Demo battery's fallback (K11), and the only
    /// route an HMAC key can sign (R22).
    pub rest_orders: bool,
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
    /// `max_leverage` (BX7, BX-19): the highest initial leverage an owned
    /// row may carry — asserted at boot, a lock when seen above it later.
    pub max_leverage: u8,
    /// The most venue inventory the boot adopts (BX7): the owner slot's
    /// instance cap, USD ×1e6 (`cap_instance_usd_1e6`). `0` adopts nothing
    /// (a position on an owned row refuses the boot).
    pub adopt_cap_usd_1e6: i64,
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
    /// An HMAC key on the WS API route: HMAC signs REST only (R22; the WS
    /// API's `session.logon` takes Ed25519 keys only) — or (BX9, Q-1) options
    /// armed with no HMAC key to sign them (eapi refuses an Ed25519 key).
    Signing,
    /// A bound row of a product no lane serves in the account's mode
    /// (`PRODUCT_*`; BX7, M3: USDⓈ-M and COIN-M; BX8: spot, classic only;
    /// BX9: options, classic only).
    Product(u8),
    /// The venue holds positions on owned rows whose exposure in all (USD
    /// ×1e6; `i64::MAX` when one cannot be priced) does not fit the owner
    /// slot's instance cap — or more than lane 4 and the held queue can
    /// take at boot: nothing is adopted (BX7).
    Inventory(i64),
    /// Spot on the REST order route or with an HMAC key (BX8): spot's user
    /// data exists only on a WS API session, whose logon takes Ed25519
    /// keys only.
    SpotRoute,
    /// A spot row (its index) whose base asset another armed spot row
    /// shares, or names as its quote — or past [`SPOT_ASSETS_MAX`] rows
    /// (BX8: the reconciled asset is the row's).
    SpotAsset(u16),
    /// An order of ours — a ghost, or another epoch's — rests on a spot
    /// row (its index) at boot: no spot cancel is built (BX-17), so the
    /// operator cancels it in the app (BX8).
    SpotStray(u16),
    /// An order of ours rests on an option row (its index) at boot: no
    /// options cancel is built (IoC-only, BX-17) — the operator cancels it
    /// in the app (BX9).
    OptStray(u16),
    /// The account holds a short on an owned option row (its index) the arm
    /// may not write (BX-18: not writable, or not owned to be adopted): the
    /// boot is refused (BX9) — the operator closes it, or arms writing.
    OptionShort(u16),
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
    /// Portfolio Margin's unified account (`/papi/v1/account`): the ratio,
    /// the session equity, the account status (BX7, C2).
    PmAccount = 12,
    /// An owned USDⓈ-M row's margin type and leverage
    /// (`/fapi/v1/symbolConfig`; BX7, BX-19): boot only.
    SymbolConfig = 13,
    /// A spot row's price (`/api/v3/ticker/price`; BX8): a balance the
    /// boot adopts is valued at it — boot only.
    Price = 14,
    /// A spot order's trades (`/api/v3/myTrades?orderId=`; BX8): read after
    /// a fill of it was booked without its report, for the commissions
    /// nothing else carries.
    OrderTrades = 15,
    /// BX9: the options positions (`/eapi/v1/position`) — the cycle's legs.
    OptPositions = 16,
    /// BX9: the exercise records (`/eapi/v1/exerciseRecord`) — read while an
    /// expired leg is settling.
    Exercise = 17,
    /// BX9 (Q-1): the options key's own permissions (`apiRestrictions`
    /// under its header) when it is not the arm's: boot only, SAPI.
    RestrictionsOpt = 18,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Job {
    kind: JobKind,
    row: u16,
    /// The lane whose REST connection carries it.
    lane: u8,
    sent_ns: u64,
    /// [`JobKind::OrderTrades`]: the venue's order id; 0 otherwise.
    oid: u64,
    /// [`JobKind::OrderTrades`]: the tries so far (a failed read is asked
    /// again, up to [`ORDER_TRADES_TRIES`]).
    tries: u8,
    /// [`JobKind::OrderTrades`]: the order's executed quantity ×1e6 — its
    /// trades in the answer must sum to it, or the read is asked again (the
    /// venue's trade list may lag its match: the risk review's R2-3); 0
    /// otherwise.
    qty_1e6: i64,
    /// [`JobKind::OrderTrades`] asked again: not before this time — the
    /// venue's list may lag its match longer than a round trip (the e2e
    /// review); 0 otherwise.
    not_before_ns: u64,
}

/// The tries a spot order's trades get (BX8): a commission unread after
/// them stays unpaid — drift, fail closed.
const ORDER_TRADES_TRIES: u8 = 3;

/// The pause before a spot order's trades are asked again, × the tries so
/// far (BX8): 1 s, then 2 s — a lagging list gets three seconds, its rows'
/// legs held meanwhile.
const ORDER_TRADES_BACKOFF_NS: u64 = 1_000_000_000;

/// The positions one account read may list (BX7). Portfolio Margin's
/// `GET /papi/v2/um/account` and dapi's account list EVERY symbol of their
/// market (some 600 and 60 today), flat ones included; a read past this is
/// refused (`Truncated`: the cycle fails, fail closed), never cut short.
const POS_CAP: usize = 2_048;

/// REST jobs queued at once. Each row queues at most one countdown, one
/// day read and one REST cancel-all at a time — and a row's symbol-config
/// read (BX7) is the Assert phase's alone, answered before any of those —
/// so the queue holds every such job there can be. Spot's order's-trades
/// reads (BX8) are one per fill booked without its report: a burst of
/// them past the room is counted ([`crate::arm::GW_JOBS_DROPPED`]) and
/// their commissions read as drift (fail closed).
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
            // On the heap from the start (never a stack temporary).
            buf: exec_gateway::boxed_array(None),
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
    /// The `k`-th queued from the front (a cold walk).
    #[inline(always)]
    fn get(&self, k: usize) -> Option<&T> {
        if k < self.len() {
            self.buf[(self.head + k) % N].as_ref()
        } else {
            None
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
    /// Portfolio Margin's and PM Pro's unified account, the cycle's last
    /// step: no verdict without it (BX7).
    Unified,
    /// BX9: the options positions (`/eapi/v1/position`), between the
    /// options account and the open orders.
    Positions,
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
    /// The last account read that listed the row (its generation, BX7): a
    /// read listing it twice fails; a COIN-M read not listing an owned row
    /// is judged.
    listed: u32,
    /// Spot (BX8): the base asset's balance the row's bookings explain,
    /// ×1e8 (the module docs' balance law).
    spot_1e8: i64,
    /// Spot: the row's index in the armed assets (`u8::MAX`: not spot).
    spot_k: u8,
}

/// One WS API request in flight: the in-flight table's record, in place.
#[derive(Copy, Clone, Debug)]
struct InFlight {
    req: WsReq,
    /// The order session it went out on, [`sess`]`(lane, slot)` (its
    /// answer comes back on it; its loss is that session's).
    conn: u8,
    /// Its answer's deadline in the queue (S2); `None` only for a cancel
    /// sent while the queue was full (XG-3).
    deadline: Option<TimerId>,
}

impl InFlight {
    const BLANK: Self = Self {
        req: WsReq::on(0, IX_NONE, 0, 0),
        conn: 0,
        deadline: None,
    };
}

/// One request the REST order route carries (BX7, C5): the order it is
/// for — rendered when its turn comes — and, once on the wire, when.
#[derive(Copy, Clone, Debug)]
struct RestReq {
    req: WsReq,
    sent_ns: u64,
}

/// **The REST order route** (BX7, C5; `order_transport = "rest"`): its own
/// keep-alive connection, one request on the wire at a time, the rest
/// waiting in order. It is up while its last request was answered: a
/// request the transport lost (a timeout, a dead socket) takes it down
/// until the next answer — nothing new is admitted and the dead-man is
/// not renewed (S2's law, for REST) — and what still waited never left.
struct RestRoute {
    conn: HttpsConn,
    queue: Fifo<RestReq, REST_Q>,
    wire: Option<RestReq>,
    up: bool,
    /// When it last delivered an answer (S2's probe).
    rx_ns: u64,
}

/// **One futures product's connections** (BX7, M3): its WS API order
/// sessions under make-before-break — or its REST order route — its
/// user-data stream and listenKey, and its REST jobs connection and queue.
/// The lanes share everything else: the tables, the held queue, fill
/// lane 4, the journal, the stats block, the signer, the clock, the desk
/// and the weight window (one IP pool).
struct Lane {
    /// `PRODUCT_USDM`, `PRODUCT_COINM`, `PRODUCT_SPOT` or `PRODUCT_OPTIONS`.
    product: u8,
    /// Its hosts' indices in the resolver (BX7, DNS): the WS API's, the
    /// stream host's (the user stream and PM Pro's risk stream), the REST
    /// host's (the jobs and the order route).
    hosts: [u8; 3],
    /// A Portfolio Margin lane (BX7, C2): papi, REST only, no stream of its
    /// own but the first lane's.
    pm: bool,
    /// The margin is the account's, not the lane's (Portfolio Margin and PM
    /// Pro, BX7 C2/C3): the lane's account step reads positions; the ratio,
    /// the equity and the status come from the unified account.
    unified: bool,
    /// PM Pro's risk stream, on the USDⓈ-M lane (BX7, C3).
    risk: Option<RiskStream>,
    /// The lane holds a user stream (classic futures: every lane; Portfolio
    /// Margin: the first — the account has ONE, its events naming their
    /// product; spot: none — its user data rides the order sessions).
    has_us: bool,
    /// The stream's path before the key: `/ws/`, PM's `/pm/ws/`, or
    /// options' `/private/ws/` (BX9).
    us_prefix: &'static [u8],
    /// The lane's request statics (papi's COIN-M order takes no STP mode).
    st: WsStatic,
    /// The order sessions' two slots (make-before-break, [`Mbb`]).
    ws: [WsConn; 2],
    mbb: Mbb,
    /// When each slot's socket opened (the rotation's age).
    opened_ns: [u64; 2],
    /// When each slot last delivered a frame (S2).
    rx_ns: [u64; 2],
    /// Order-session sockets opened this process (the first is no
    /// reconnect).
    ws_opens: u64,
    /// Since when the lane's order route has been down — no WS API session
    /// logged on, or the REST route's last request lost (0: up). The
    /// pulse's gap (the arm's disconnect halt) reads it.
    ord_down_ns: u64,
    /// The REST order route, when it replaces the WS API (BX7, C5).
    route: Option<RestRoute>,
    us: WsConn,
    user: UserStream,
    us_down_ns: u64,
    us_backoff: Backoff,
    us_retry_ns: u64,
    rest: HttpsConn,
    jobs: Fifo<Job, JOB_CAP>,
    job: Option<Job>,
    /// The product's last margin ratio ×1e6 (BX-20; COIN-M: the riskiest
    /// asset's).
    ratio_1e6: i64,
    /// When the lane's account was last asked for: a margin re-read starts
    /// no sooner than [`ACCOUNT_REREAD_NS`] after it.
    account_read_ns: u64,
    /// A margin re-read is due at this time (0: none asked for).
    account_next_ns: u64,
}

/// **PM Pro's risk stream** (BX7, C3): `/pm-classic/ws/<listenKey>` on
/// the USDⓈ-M stream host, on the USDⓈ-M lane's own key (`POST
/// /fapi/v1/listenKey`). It carries `riskLevelChange` (a margin call, or a
/// lock) and the 5 s `PM_PRO_ACCOUNT_UPDATE` (not read: the cycle reads the
/// account). Risk guidance only: its outage gates nothing — the unified
/// account's status is read every cycle.
struct RiskStream {
    ws: WsConn,
    backoff: Backoff,
    retry_ns: u64,
}

/// What starting the REST route's next request did.
enum RestStart {
    /// On the wire.
    Started,
    /// Not now (a dial pending, the connection down): it waits at the front.
    Later,
    /// It did not fit its window: it never leaves.
    Unrendered,
    /// The connection cannot send: the route is down; it waits at the front.
    Down,
}

/// Where an answer's bytes lie.
#[derive(Copy, Clone)]
enum Frame {
    /// A WS API frame in lane `.0`'s session `.1`'s window.
    Ws(usize, usize, core_net::PayloadSpan),
    /// Lane `.0`'s REST route's body, `.1 .. .2`.
    Rest(usize, usize, usize),
}

impl Frame {
    /// The lane the answer came from.
    #[inline(always)]
    const fn lane(self) -> usize {
        match self {
            Self::Ws(l, _, _) | Self::Rest(l, _, _) => l,
        }
    }
}

/// A WS API or REST-route request's weight class on `product`'s API
/// (gov's table; BX9: options' account is `marginAccount`).
#[inline(always)]
const fn verb_weight(verb: u8, product: u8) -> Weighed {
    match verb {
        REQ_PLACE => Weighed::Place,
        REQ_ACCOUNT if product == PRODUCT_OPTIONS => Weighed::OptAccount,
        REQ_ACCOUNT => Weighed::Account,
        REQ_LOGON => Weighed::Logon,
        _ => Weighed::OrderVerb,
    }
}

/// A REST job's weight class on `product`'s API.
#[inline(always)]
const fn job_weight(k: JobKind, product: u8) -> Weighed {
    match k {
        JobKind::Dual | JobKind::MultiAssets => Weighed::ModeRead,
        JobKind::Countdown | JobKind::Probe => Weighed::Countdown,
        JobKind::OpenOrders => Weighed::OpenOrders,
        JobKind::UserTrades if product == PRODUCT_COINM => Weighed::UserTradesCm,
        JobKind::UserTrades | JobKind::OrderTrades => Weighed::UserTrades,
        JobKind::PmAccount => Weighed::PmAccount,
        JobKind::SymbolConfig => Weighed::SymbolConfig,
        JobKind::OptPositions | JobKind::Exercise => Weighed::OptPosition,
        JobKind::Time
        | JobKind::Restrictions
        | JobKind::RestrictionsOpt
        | JobKind::Portfolio
        | JobKind::ListenNew
        | JobKind::ListenKeep
        | JobKind::CancelAll
        | JobKind::Price => Weighed::One,
    }
}

/// `h`'s index among the first `*n` of `names`, appended if new (BX7, DNS).
fn host_index(names: &mut [&'static str; DNS_HOSTS], n: &mut usize, h: &'static str) -> Result<u8, BootErr> {
    let mut i = 0;
    while i < *n {
        if names[i] == h {
            return Ok(i as u8);
        }
        i += 1;
    }
    if *n == DNS_HOSTS {
        return Err(BootErr::Transport);
    }
    names[*n] = h;
    *n += 1;
    Ok((*n - 1) as u8)
}

/// An answer's bytes: a WS API frame still in its session's window, or the
/// REST route's body — borrowing only those two fields, so a scan can fill
/// the gateway's own tables from it.
#[inline]
fn frame_of(lanes: &[Lane], f: Frame) -> &[u8] {
    match f {
        Frame::Ws(l, s, span) => lanes[l].ws[s].payload(span),
        Frame::Rest(l, b0, b1) => match &lanes[l].route {
            Some(rt) => rt.conn.resp().get(b0..b1).unwrap_or(&[]),
            None => &[],
        },
    }
}

/// XG-3: the deadline queue is full — a sizing bug ([`TIMER_CAP`] covers
/// every order and request): the halt word, and fatal in debug.
#[cold]
fn timer_full(stats: &GatewayStats) {
    stats.halt(HALT_TIMER_FULL);
    debug_assert!(false, "the gateway's deadline queue is full");
}

/// The requests in flight and the deadlines — apart from the gateway's
/// other fields, so a request can be issued while an answer it renders
/// from is still borrowed (a stray's cancel names the venue's own id).
struct Desk {
    /// The WS API requests in flight, by id (records in place).
    pending: Pending<InFlight, WS_IDS>,
    /// Every deadline: TTL and owed cancels, WS API answers.
    tq: TimerQueue<TIMER_CAP>,
    /// Requests in flight per order session, by [`sess`] (a draining one is
    /// closed at 0).
    conn_live: [u16; 2 * LANES],
}

impl Desk {
    /// A WS API request id for `req` on order session `conn` ([`sess`]), recorded in
    /// place with its answer's deadline (S2). `Err` carries the local
    /// code: every id is in flight — or, for anything but a cancel, the
    /// deadline queue is full (XG-3: what cannot be timed out is not sent;
    /// a cancel always is).
    fn issue(&mut self, stats: &GatewayStats, req: WsReq, conn: usize, now: u64) -> Result<u64, i32> {
        let Some((id, rec)) = self.pending.claim_next(now) else {
            return Err(CODE_IDS_FULL);
        };
        *rec = InFlight {
            req,
            conn: conn as u8,
            deadline: None,
        };
        match self.tq.arm(now + WS_REQ_TIMEOUT_NS, id, TK_WS_REQ) {
            Ok(tid) => {
                if let Some(r) = self.pending.get_mut(id) {
                    r.deadline = Some(tid);
                }
            }
            Err(_) => {
                timer_full(stats);
                if req.verb != REQ_CANCEL {
                    self.pending.release(id);
                    return Err(CODE_TIMER_FULL);
                }
            }
        }
        self.conn_live[conn] += 1;
        Ok(id)
    }

    /// Request `id` leaves the table — answered, lost, or never written:
    /// its record and send time; its deadline cancelled, its session's
    /// count dropped. `None`: not in flight (late, duplicate or foreign).
    fn take(&mut self, id: u64) -> Option<(InFlight, u64)> {
        let r = *self.pending.get(id)?;
        let sent = self.pending.sent_ns(id).unwrap_or(0);
        self.pending.release(id);
        if let Some(d) = r.deadline {
            // A popped deadline is already gone: the cancel is a no-op.
            self.tq.cancel(d);
        }
        let c = r.conn as usize;
        self.conn_live[c] = self.conn_live[c].saturating_sub(1);
        Some((r, sent))
    }
}

/// **The gateway** (module docs). `FILL_N` is the engine's fill-lane size;
/// `J_N` the journal ring's (BX11: the engine's [`JOURNAL_RING`]).
pub struct BnGateway<const FILL_N: usize, const J_N: usize = JOURNAL_RING> {
    cmd: CmdRx<BnCmd, CMD_RING>,
    evt: EvtTx<BnEvt, EVT_RING>,
    /// The duplex's shared block: the sweep and halt words, the heartbeat,
    /// the gauges, the published tallies, the latencies.
    stats: Arc<GatewayStats>,
    fills: Producer<Fill, FILL_N>,
    journal: JournalTx<J_N>,
    poll: Poll,
    events: Events,
    /// The futures lanes (BX7, M3), one per armed product.
    lanes: Box<[Lane]>,
    /// `PRODUCT_*` → its lane (`NO_LANE`: not armed).
    lane_ix: [u8; crate::inst::PRODUCTS],
    sapi: Option<HttpsConn>,
    /// The SAPI host's index in the resolver.
    sapi_host: u8,
    /// The cold resolver (BX7, DNS).
    dns: Resolver,
    /// The WS API requests in flight and every deadline.
    desk: Desk,
    /// Each open order's deadline in the queue (TTL or owed cancel; at most
    /// one), and when it falls.
    order_tid: Box<[Option<TimerId>; OOT_CAP]>,
    order_at: Box<[u64; OOT_CAP]>,
    /// The loop's wait policy, and whether the last pass's sockets worked.
    idle: Idle,
    worked: bool,
    /// The push times of the commands rendered this burst (`push_to_write`,
    /// recorded at the flush).
    burst_pushed: [u64; CMD_BURST],
    burst_n: usize,
    /// The sweep request last taken from the stats word.
    sweep_seq: u64,
    /// The REQUEST_WEIGHT spent in the last minute (BX7, §3.9's run-time
    /// governor).
    weight: RateWindow,
    /// eapi's REQUEST_WEIGHT spent in the last minute (BX9: options' own
    /// pool, metered apart).
    opt_weight: RateWindow,
    /// The arm's one signer: Ed25519, or HMAC on the REST route (R22).
    signer: Signer,
    api_key: String,
    /// BX9 (Q-1): the options lane's signer — the scope's HMAC key (eapi
    /// takes no Ed25519 key) — and whether its key is not the arm's (its
    /// permissions are then read apart, [`JobKind::RestrictionsOpt`]).
    /// `None`: no options armed.
    opt_signer: Option<Signer>,
    opt_own_key: bool,
    /// The account is Portfolio Margin (BX7, C2): every lane is papi's.
    pm: bool,
    /// The account is PM Pro (BX7, C3): classic lanes, the unified account
    /// on SAPI (`/sapi/v1/portfolio/account`), the risk stream.
    pm_pro: bool,
    /// Portfolio Margin's unified account read is queued or in flight.
    pm_account_queued: bool,
    /// The configuration is `Scope::Demo` (BX7b): classic by construction,
    /// no SAPI reads at boot (module docs).
    demo: bool,
    prefix: CidPrefix,
    wire: WireTable,
    oot: Oot,
    tids: TidRing<256>,
    ended: EndedRing,
    clock: VenueClock,
    knobs: GwKnobs,
    phase: Phase,
    rows: Vec<RowGw>,
    legs: Vec<Leg>,
    sapi_job: Option<Job>,
    held: Fifo<Out, HELD_CAP>,
    recon: ReconStep,
    recon_next_ns: u64,
    /// When the cycle at the cadence is due — the one the boot's weight
    /// model counts (`gov::arm_weight_per_min`). `recon_next_ns` may ask for
    /// one sooner (a stream's reopen); only such an early cycle waits on the
    /// weight throttle (BX7, the risk review's round-3 finding 1).
    recon_cadence_ns: u64,
    /// The recon in flight reads the open orders alone (the sweep's
    /// confirmation while the order session is down).
    recon_listing_only: bool,
    /// The recon in flight reads the account alone (a margin re-read, S5;
    /// the order session's probe, S2).
    recon_account_only: bool,
    /// The lane whose step is in flight: a cycle reads every lane's account
    /// and open orders, in lane order, before one verdict (BX7, M3).
    recon_lane: u8,
    /// Our orders the cycle's listings named open, every lane so far (the
    /// sweep's confirmation).
    recon_ours: u32,
    pos: Box<[PosRow; POS_CAP]>,
    orders: Box<[OrderRow; 1_024]>,
    trades: Box<[TradeRow; TRADES_PAGE]>,
    /// The account reads' generation (each read's rows are marked with it;
    /// never 0, the rows' blank).
    read_gen: u32,
    /// The cycle's verdict, accumulated lane by lane.
    verdict: Verdict,
    /// The session equity (E7): USDⓈ-M's margin balance plus the options
    /// account's equity (BX9, Q-4) — 0 until each armed one is read
    /// ([`Self::refresh_equity`]; COIN-M has no USD equity) — or the unified
    /// account's (PM, PM Pro).
    equity_1e6: i64,
    /// The parts of a classic session equity, as last read, and whether read.
    um_equity_1e6: i64,
    opt_equity_1e6: i64,
    um_read: bool,
    opt_read: bool,
    /// The session equity was read whole: every armed part (classic), or
    /// the unified account (PM, PM Pro). Until then the anchor reaches the
    /// arm as 0 — no session result (the risk review's R5: a restart's
    /// anchor beside an unread part read as that part's loss).
    equity_known: bool,
    /// BX9: an options account lock was raised this process (once:
    /// [`CODE_OPT_ACCOUNT`]).
    opt_locked: bool,
    /// BX9: each option underlying's index ×1e6 (`/eapi/v1/index`, read at
    /// boot for a held short's adoption; 0: unread).
    opt_index_1e6: [i64; OPT_ULY_MAX],
    /// BX9: the exercise records' read is queued or in flight, and the
    /// earliest settling expiry it asks from (ms).
    exercise_queued: bool,
    exercise_from_ms: u64,
    exercise: Box<[ExerciseRow; EXERCISE_PAGE]>,
    anchor_1e6: i64,
    status_next_ns: u64,
    clock_next_ns: u64,
    clock_pending: u8,
    doubt_next_ns: u64,
    timers_next_ns: u64,
    asserts_left: u16,
    /// A margin-type or leverage lock was raised this session (once).
    margin_locked: bool,
    /// The boot's adoption pass ran (once, at the first full cycle).
    adopt_ran: bool,
    /// Adopted fills are on their way: the day's report waits for lane 4
    /// to deliver them (the slot is seeded only after).
    adopt_wait: bool,
    /// What the boot adopted: rows, and their exposure USD ×1e6.
    adopted_rows: u32,
    adopted_usd_1e6: i64,
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
    /// Makers of ours resting, every row.
    makers: u32,
    /// Sweeps that ended stranded in a row (their reads back off).
    sweep_streak: u8,
    /// The last sweep: orders left, cancels sent, `EVT_F_SWEEP_*`.
    last_sweep: (i64, i64, u8),
    /// The last reconciliation's verdict.
    last_verdict: Verdict,
    /// Spot (BX8): the armed base assets (one per spot row, `n_spot` of
    /// them), each one's row, and the last account read's balances ×1e8 —
    /// boxed, built where they live (the gateway's move carries pointers).
    spot_assets: Box<[AssetName; SPOT_ASSETS_MAX]>,
    spot_rows: Box<[u16; SPOT_ASSETS_MAX]>,
    n_spot: usize,
    spot_bal_1e8: Box<[i64; SPOT_ASSETS_MAX]>,
    /// The spot account's `permissions`, as last read.
    acct_perm: BnPermissions,
    /// A permission lock was raised this process (once: [`CODE_PERMISSION`]).
    perm_locked: bool,
}

impl<const FILL_N: usize, const J_N: usize> BnGateway<FILL_N, J_N> {
    /// **Boot-only construction**: resolve every host (blocking DNS, here
    /// and never again), build the transports and the ONE signer (after the
    /// signer's own self-test and the boot vectors).
    pub fn new(
        cfg: &BnConfig,
        tls: Arc<rustls::ClientConfig>,
        knobs: GwKnobs,
        wire: WireTable,
        end: BnGatewayEnd,
        fills: Producer<Fill, FILL_N>,
        journal: JournalTx<J_N>,
    ) -> Result<Self, BootErr> {
        crate::selftest::run().map_err(BootErr::SelfTest)?;
        let signer = cfg.signer().map_err(|_| BootErr::SelfTest(crate::selftest::SelfTestErr::Signer))?;
        // Portfolio Margin (BX7, C2): papi, REST only, every lane. PM Pro
        // (C3): classic lanes, the unified account on SAPI.
        let pm = knobs.mode == AccountMode::Pm;
        let pm_pro = knobs.mode == AccountMode::PmPro;
        // BX7b: Demo offers no Portfolio Margin — a Demo account is classic
        // by construction, and that is the mode assertion it gets.
        let demo = cfg.scope() == Scope::Demo;
        if demo && knobs.mode != AccountMode::Classic {
            return Err(BootErr::Assert(AssertErr::Mode));
        }
        let key_header = [(KEY_HEADER, cfg.api_key())];
        // The lanes: one per product the wire table binds, USDⓈ-M first,
        // options last (BX7, M3; BX8; BX9); a row-less arm keeps USDⓈ-M's. A
        // row of any other product refuses the boot — and spot or options
        // outside classic.
        // Spot's armed base assets (BX8): one row each, none another row's
        // quote (the module docs' balance law).
        let mut want = [false; crate::inst::PRODUCTS];
        let mut spot_assets: Box<[AssetName; SPOT_ASSETS_MAX]> = exec_gateway::boxed_array(AssetName::EMPTY);
        let mut spot_rows: Box<[u16; SPOT_ASSETS_MAX]> = exec_gateway::boxed_array(ROW_NONE);
        let mut n_spot = 0usize;
        let mut i = 0;
        while i < wire.len() {
            let p = wire.row(i as u16).product;
            let spot = p == PRODUCT_SPOT;
            let classic_only = spot || p == PRODUCT_OPTIONS;
            if (p != PRODUCT_USDM && p != PRODUCT_COINM && !classic_only) || (classic_only && (pm || pm_pro)) {
                return Err(BootErr::Product(p));
            }
            if spot {
                if n_spot == SPOT_ASSETS_MAX {
                    return Err(BootErr::SpotAsset(i as u16));
                }
                // COPY: the row's base-asset name (17 B), boot only, once per
                // spot row, into the boxed array the gateway keeps (never
                // moved again) — the per-event `spot_row_of` and the account
                // scan read one contiguous array — rejected: walking the
                // wire table's spot rows per event (a stride of 112 B).
                spot_assets[n_spot] = wire.spot(i as u16).base;
                spot_rows[n_spot] = i as u16;
                n_spot += 1;
            }
            want[p as usize] = true;
            i += 1;
        }
        let mut a = 0;
        while a < n_spot {
            let mut b = 0;
            while b < n_spot {
                if (a != b && spot_assets[a] == spot_assets[b]) || wire.spot(spot_rows[b]).quote == spot_assets[a] {
                    return Err(BootErr::SpotAsset(spot_rows[a]));
                }
                b += 1;
            }
            a += 1;
        }
        let opts = want[PRODUCT_OPTIONS as usize];
        if !want[PRODUCT_USDM as usize] && !want[PRODUCT_COINM as usize] && !want[PRODUCT_SPOT as usize] && !opts {
            want[PRODUCT_USDM as usize] = true;
        }
        // R22: HMAC signs REST only — the WS API's `session.logon` takes
        // Ed25519 keys only: an HMAC key arms no lane on the WS API (the
        // options lane, REST-only, signs with its own, below).
        let ws_lane = (want[PRODUCT_USDM as usize] || want[PRODUCT_COINM as usize] || want[PRODUCT_SPOT as usize]) && !knobs.rest_orders && !pm;
        if signer.signing() == Signing::Hmac && ws_lane {
            return Err(BootErr::Signing);
        }
        // BX8: spot's user data exists only on a WS API session, whose
        // logon takes Ed25519 keys only.
        if want[PRODUCT_SPOT as usize] && (knobs.rest_orders || signer.signing() == Signing::Hmac) {
            return Err(BootErr::SpotRoute);
        }
        // BX9 (Q-1): options sign with the scope's HMAC key — none, and the
        // boot is refused (eapi refuses an Ed25519 key). Its header is the
        // options key's; its permissions are read apart when it is not the
        // arm's key.
        let opt_signer = if opts {
            match cfg.opt_signer() {
                Ok(Some(s)) => Some(s),
                Ok(None) => return Err(BootErr::Signing),
                Err(_) => return Err(BootErr::SelfTest(crate::selftest::SelfTestErr::Signer)),
            }
        } else {
            None
        };
        let opt_key = cfg.opt_api_key().unwrap_or(cfg.api_key());
        let opt_own_key = opts && opt_key != cfg.api_key();
        let opt_header = [(KEY_HEADER, opt_key)];
        let now = core_time::now_ns();
        let st = WsStatic::new(&knobs.stp_mode, knobs.recv_window_ms);
        let mut lane_ix = [NO_LANE; crate::inst::PRODUCTS];
        // Reserved at the count armed: the box below then takes the list's
        // allocation as it is (no spare slot for a shrink to move it from).
        let mut armed = 0;
        let mut k = 0;
        while k < LANES {
            armed += usize::from(want[LANE_ORDER[k] as usize]);
            k += 1;
        }
        let mut lanes: Vec<Lane> = Vec::with_capacity(armed);
        let mut k = 0;
        while k < LANES {
            let p = LANE_ORDER[k];
            if want[p as usize] {
                lane_ix[p as usize] = lanes.len() as u8;
                let header: &[(&str, &str)] = if p == PRODUCT_OPTIONS { &opt_header } else { &key_header };
                let lane = Self::build_lane(p, lanes.len(), cfg, &tls, header, &knobs, now, st, pm, pm_pro)?;
                // COPY: the lane (a few KiB, its sockets' state inline) moved
                // into the list — boot only, once per lane — rejected: building
                // it in place (it is assembled from fallible constructors).
                lanes.push(lane);
            }
            k += 1;
        }
        debug_assert_eq!(lanes.len(), lanes.capacity(), "the lanes reserved at their count");
        let mut lanes = lanes.into_boxed_slice();
        let sapi = HttpsConn::new(&cfg.hosts().sapi, cfg.port(), tls, &sapi_specs(&key_header, &opt_header), CONN_CFG, TOKEN_SAPI)
            .map_err(|_| BootErr::Transport)?;
        let (dns, sapi_host) = Self::resolver(&mut lanes, &sapi, cfg.port())?;
        let poll = Poll::new().map_err(|_| BootErr::Transport)?;
        let n = wire.len();
        let mut legs = vec![Leg::default(); n];
        let mut rows = vec![
            RowGw {
                spot_k: u8::MAX,
                ..RowGw::default()
            };
            n
        ];
        let mut k = 0;
        while k < n_spot {
            rows[spot_rows[k] as usize].spot_k = k as u8;
            k += 1;
        }
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
        let idle = if knobs.spin {
            Idle::new(0, 0)
        } else {
            Idle::default()
        };
        Ok(Self {
            cmd: end.cmd,
            evt: end.evt,
            stats: end.stats,
            fills,
            journal,
            poll,
            events: Events::with_capacity(64),
            lanes,
            lane_ix,
            sapi: Some(sapi),
            sapi_host,
            dns,
            desk: Desk {
                pending: Pending::new(InFlight::BLANK),
                tq: TimerQueue::new(),
                conn_live: [0; 2 * LANES],
            },
            order_tid: exec_gateway::boxed_array(None),
            order_at: exec_gateway::boxed_array(0u64),
            idle,
            worked: false,
            burst_pushed: [0; CMD_BURST],
            burst_n: 0,
            sweep_seq: 0,
            weight: RateWindow::new(),
            opt_weight: RateWindow::new(),
            signer,
            api_key: String::from(cfg.api_key()),
            opt_signer,
            opt_own_key,
            pm,
            pm_pro,
            pm_account_queued: false,
            demo,
            prefix: CidPrefix::new(knobs.epoch),
            wire,
            oot: Oot::new(),
            tids: TidRing::new(),
            ended: EndedRing::new(),
            clock: VenueClock::new(core_time::WallAnchor::now()),
            anchor_1e6: knobs.pnl_anchor_1e6,
            knobs,
            phase: Phase::Clock,
            rows,
            legs,
            sapi_job: None,
            held: Fifo::new(),
            recon: ReconStep::Idle,
            recon_next_ns: 0,
            recon_cadence_ns: 0,
            recon_listing_only: false,
            recon_account_only: false,
            recon_lane: 0,
            recon_ours: 0,
            pos: exec_gateway::boxed_array(PosRow::default()),
            orders: exec_gateway::boxed_array(OrderRow::default()),
            trades: exec_gateway::boxed_array(TradeRow::default()),
            read_gen: 0,
            verdict: Verdict::default(),
            equity_1e6: 0,
            um_equity_1e6: 0,
            opt_equity_1e6: 0,
            um_read: false,
            opt_read: false,
            equity_known: false,
            opt_locked: false,
            opt_index_1e6: [0; OPT_ULY_MAX],
            exercise_queued: false,
            exercise_from_ms: 0,
            exercise: exec_gateway::boxed_array(ExerciseRow::default()),
            status_next_ns: 0,
            clock_next_ns: 0,
            clock_pending: 0,
            doubt_next_ns: 0,
            timers_next_ns: 0,
            asserts_left: 0,
            margin_locked: false,
            adopt_ran: false,
            adopt_wait: false,
            adopted_rows: 0,
            adopted_usd_1e6: 0,
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
            makers: 0,
            sweep_streak: 0,
            last_sweep: (0, 0, 0),
            last_verdict: Verdict::default(),
            spot_assets,
            spot_rows,
            n_spot,
            spot_bal_1e8: exec_gateway::boxed_array(0),
            acct_perm: BnPermissions::EMPTY,
            perm_locked: false,
        })
    }

    /// **One lane's connections** (boot only): its WS API pair, its user
    /// stream and its REST jobs connection — and its REST order route when
    /// `order_transport = "rest"` — on the product's hosts and paths. A
    /// Portfolio Margin lane (`pm`, BX7 C2) is papi's: the REST route
    /// always (papi has no WS API: the pair is never dialled), the
    /// account's ONE stream on the first lane (`/pm/ws/`). Spot's (BX8)
    /// holds no stream: its sessions carry the user data (the stream socket
    /// is built, never dialled). Options' (BX9) are eapi's: the REST route
    /// always (no WS API), the stream `/private/ws/` — `key_header` is the
    /// options key's.
    #[allow(clippy::too_many_arguments)]
    fn build_lane(
        product: u8,
        l: usize,
        cfg: &BnConfig,
        tls: &Arc<rustls::ClientConfig>,
        key_header: &[(&str, &str)],
        knobs: &GwKnobs,
        now: u64,
        st: WsStatic,
        pm: bool,
        pm_pro: bool,
    ) -> Result<Lane, BootErr> {
        let h = if pm { cfg.hosts().pm_lane() } else { cfg.hosts().lane(product) };
        let port = cfg.port();
        // Each socket's masking seed: the epoch, the lane, the role.
        let seed = u64::from(knobs.epoch) ^ ((l as u64) << 40);
        let ws0 = WsConn::new(h.wsapi, port, h.wsapi_path, tls.clone(), WS_CFG, lane_token(l, TK_WS0), 0x6273_7773 ^ seed)
            .map_err(|_| BootErr::Transport)?;
        let ws1 = WsConn::new(h.wsapi, port, h.wsapi_path, tls.clone(), WS_CFG, lane_token(l, TK_WS1), 0x6273_7731 ^ seed)
            .map_err(|_| BootErr::Transport)?;
        // The user stream's path is the listenKey's, set once it exists.
        let opt = product == PRODUCT_OPTIONS;
        let (us_path, us_prefix): (&str, &'static [u8]) = if pm {
            ("/pm/ws/pending", b"/pm/ws/")
        } else if opt {
            ("/private/ws/pending", b"/private/ws/")
        } else {
            ("/ws/pending", b"/ws/")
        };
        let us = WsConn::new(h.user, port, us_path, tls.clone(), WS_CFG, lane_token(l, TK_US), 0x6273_7573 ^ seed)
            .map_err(|_| BootErr::Transport)?;
        let rest = HttpsConn::new(h.rest, port, tls.clone(), &lane_specs(product, pm, key_header), CONN_CFG, lane_token(l, TK_REST))
            .map_err(|_| BootErr::Transport)?;
        let rest_route = knobs.rest_orders || pm || opt;
        let route = if rest_route {
            let conn = HttpsConn::new(h.rest, port, tls.clone(), &ord_specs(product, pm, key_header), CONN_CFG, lane_token(l, TK_ORD))
                .map_err(|_| BootErr::Transport)?;
            Some(RestRoute {
                conn,
                queue: Fifo::new(),
                wire: None,
                up: true,
                rx_ns: 0,
            })
        } else {
            None
        };
        let j = l as u64;
        let risk = if pm_pro && product == PRODUCT_USDM {
            let ws = WsConn::new(h.user, port, "/pm-classic/ws/pending", tls.clone(), WS_CFG, lane_token(l, TK_RISK), 0x6273_7272 ^ seed)
                .map_err(|_| BootErr::Transport)?;
            Some(RiskStream {
                ws,
                backoff: Backoff::new(250 * MS, 30 * S, 13 + j),
                retry_ns: 0,
            })
        } else {
            None
        };
        Ok(Lane {
            product,
            // Set once every lane is built (`Self::resolver`).
            hosts: [0; 3],
            pm,
            unified: pm || pm_pro,
            risk,
            has_us: product != PRODUCT_SPOT && (!pm || l == 0),
            us_prefix,
            st: if pm && product == PRODUCT_COINM { st.without_stp() } else { st },
            ws: [ws0, ws1],
            mbb: Mbb::new(Backoff::new(250 * MS, 30 * S, 7 + j), WS_HEALTHY_NS, WS_DIAL_BUDGET_NS, WS_DRAIN_BUDGET_NS),
            opened_ns: [0; 2],
            rx_ns: [0; 2],
            ws_opens: 0,
            // The REST route starts up (its first answer keeps it so).
            ord_down_ns: if rest_route { 0 } else { now },
            route,
            us,
            user: UserStream::new(),
            us_down_ns: now,
            us_backoff: Backoff::new(250 * MS, 30 * S, 11 + j),
            us_retry_ns: 0,
            rest,
            jobs: Fifo::new(),
            job: None,
            ratio_1e6: 0,
            account_read_ns: 0,
            account_next_ns: 0,
        })
    }

    /// The distinct hosts the gateway dials, indexed — each lane's three,
    /// SAPI's, the connections' own interned names (no copy) — and the cold
    /// resolver over them (BX7, DNS).
    fn resolver(lanes: &mut [Lane], sapi: &HttpsConn, port: u16) -> Result<(Resolver, u8), BootErr> {
        let mut names = [""; DNS_HOSTS];
        let mut n = 0usize;
        let mut l = 0;
        while l < lanes.len() {
            let (w, u, r) = (lanes[l].ws[0].host(), lanes[l].us.host(), lanes[l].rest.host());
            lanes[l].hosts = [
                host_index(&mut names, &mut n, w)?,
                host_index(&mut names, &mut n, u)?,
                host_index(&mut names, &mut n, r)?,
            ];
            l += 1;
        }
        let sapi_host = host_index(&mut names, &mut n, sapi.host())?;
        // The one table, by value to the resolver's thread (every host is
        // on `port`; the names are the connections' interned ones).
        let dns = Resolver::spawn(names, n, port).map_err(|_| BootErr::Transport)?;
        Ok((dns, sapi_host))
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
        self.queue_job(0, JobKind::Time, 0);
        while self.phase != Phase::Ready {
            self.step();
            if let Some(e) = self.boot_err {
                return Err(e);
            }
            if core_time::now_ns().saturating_sub(start) > self.knobs.boot_timeout_ns {
                return Err(BootErr::Timeout(self.phase as u8));
            }
        }
        // The boot-only connection goes — except in Portfolio Margin, whose
        // clock reads the SAPI host's time (papi has none), and in PM Pro,
        // whose unified account is SAPI's.
        if !self.pm && !self.pm_pro {
            self.sapi = None;
        }
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
        // 0. The halt's sweep (XG-1): the word, never a command.
        let req = self.stats.sweep_requested();
        if req != self.sweep_seq && self.phase == Phase::Ready {
            self.sweep_seq = req;
            self.start_sweep(now, false);
        }
        // 1. The latency path: every queued command, one burst, one flush —
        //    none while events wait for room (XG-2).
        let mut n = 0;
        if self.phase == Phase::Ready {
            while n < CMD_BURST && self.held.len() == 0 && self.evt.room() >= EVT_ROOM_MIN {
                let Some(g) = self.cmd.pop() else {
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
        self.pump_routes(now);
        self.flush_ws();
        self.note_written();
        // 2. Sockets: the poller parks only when quiet, and never past a
        //    deadline (the kit's Idle).
        let next = match self.desk.tq.next_deadline() {
            Some(d) => d.min(self.timers_next_ns),
            None => self.timers_next_ns,
        };
        let park = self.idle.timeout_ns(n > 0 || self.worked, now, Some(next));
        self.worked = false;
        if self.poll.poll(&mut self.events, Some(Duration::from_nanos(park))).is_ok() {
            self.worked = !self.events.is_empty();
            self.dispatch_events(core_time::now_ns());
        }
        // 3. Deadlines and timers.
        self.timers(core_time::now_ns());
        self.flush_ws();
        self.drain_held();
    }

    /// `push_to_write`: the commands this burst rendered left with the
    /// flush just made.
    #[inline]
    fn note_written(&mut self) {
        if self.burst_n == 0 {
            return;
        }
        let w = core_time::now_ns();
        let mut i = 0;
        while i < self.burst_n {
            let p = self.burst_pushed[i];
            if p != 0 {
                self.stats.push_to_write.record(w.saturating_sub(p));
            }
            i += 1;
        }
        self.burst_n = 0;
    }

    // ---------------------------------------------------------------------
    // Output: fills and events in production order (obligation 1)
    // ---------------------------------------------------------------------

    fn drain_held(&mut self) {
        loop {
            let ok = match self.held.front_ref() {
                None => return,
                Some(Out::Fill(f)) => self.fills.try_push_ref(f),
                // Offered only with room: the kit's emit halts on a full
                // ring (XG-2), and a held event is not lost.
                Some(Out::Evt(e)) => self.evt.room() > 0 && self.evt.emit(e),
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
        self.output_lost();
        false
    }

    /// An output neither its ring nor the held queue could take: the arm
    /// no longer knows what rests — the halt word (XG-2), and the pulse's
    /// unbounded drift.
    #[cold]
    fn output_lost(&mut self) {
        self.lost = true;
        self.stats.halt(HALT_OUTPUT_LOST);
    }

    /// An event for the arm, behind anything held.
    #[inline]
    fn emit(&mut self, e: &BnEvt) {
        if self.held.len() == 0 && self.evt.room() > 0 {
            // Room checked: the kit's lost-event path is never taken here.
            let _ = self.evt.emit(e);
            return;
        }
        // The pulse is not held: the next carries fresher news.
        if e.kind == EVT_STATUS {
            return;
        }
        // COPY: one event (64 B) into the held queue — the ring is full, or
        // something is held ahead of it, and a retirement may not overtake
        // its fill (obligation 1) — dropping it was rejected: the arm would
        // never learn the order ended.
        if !self.held.push(Out::Evt(*e)) {
            self.output_lost();
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

    /// The lane a row's product trades on.
    #[inline(always)]
    fn lane_of_row(&self, row: u16) -> usize {
        self.lane_ix[self.wire.row(row).product as usize] as usize
    }

    /// Every lane's order route can carry a request now (the pulse's
    /// `EVT_F_ORDER_UP` and the dead-man's word: fail-closed across lanes).
    #[inline]
    fn all_ready(&self, now: u64) -> bool {
        let mut l = 0;
        while l < self.lanes.len() {
            if !self.ready_to_send(l, now) {
                return false;
            }
            l += 1;
        }
        true
    }

    /// The order session posts go out on: the active slot, logged on (the
    /// machine makes a slot active only on its logon's answer), its socket
    /// open.
    #[inline]
    fn active_ws(&self, l: usize) -> Option<usize> {
        match self.lanes[l].mbb.active() {
            Some(a) if self.lanes[l].ws[a as usize].is_open() => Some(a as usize),
            _ => None,
        }
    }

    /// The order session can take a request now: logged on, the clock
    /// measured, and not quiet after a budget or lock answer.
    #[inline]
    fn ready_to_send(&self, l: usize, now: u64) -> bool {
        self.order_up(l) && self.clock.measured() && now >= self.quiet_until_ns
    }

    /// Arm open order `ix`'s one deadline — its TTL, or an owed cancel's
    /// retry — at `at_ns`, replacing the one it has. `false`: the queue is
    /// full (XG-3; the halt word is set).
    fn arm_order(&mut self, ix: u16, at_ns: u64) -> bool {
        self.disarm_order(ix);
        let key = u64::from(ix) | (u64::from(self.oot.aux(ix).gen) << 16);
        match self.desk.tq.arm(at_ns, key, TK_ORDER) {
            Ok(tid) => {
                self.order_tid[ix as usize] = Some(tid);
                self.order_at[ix as usize] = at_ns;
                self.oot.get_mut(ix).flags |= OF_TIMED;
                true
            }
            Err(_) => {
                timer_full(&self.stats);
                false
            }
        }
    }

    /// Open order `ix` has no deadline any more.
    #[inline]
    fn disarm_order(&mut self, ix: u16) {
        if let Some(tid) = self.order_tid[ix as usize].take() {
            self.desk.tq.cancel(tid);
        }
        self.oot.get_mut(ix).flags &= !OF_TIMED;
    }


    /// The order route can carry a request: the REST route is up, or a WS
    /// API session is logged on.
    #[inline]
    fn order_up(&self, l: usize) -> bool {
        match &self.lanes[l].route {
            Some(r) => r.up,
            None => self.active_ws(l).is_some(),
        }
    }

    /// **Send `req` on the order route**: the WS API's active session, now
    /// — or the REST route's queue, rendered when its turn comes (BX7, C5).
    /// Every verb renders from the open-order table, the same for both.
    /// `Err`: the local code (not ready, no request slot, the deadline
    /// queue full, the render).
    fn post(&mut self, l: usize, req: WsReq, now: u64) -> Result<(), i32> {
        if let Some(rt) = self.lanes[l].route.as_mut() {
            return if rt.queue.push(RestReq { req, sent_ns: 0 }) { Ok(()) } else { Err(CODE_IDS_FULL) };
        }
        let Some(a) = self.active_ws(l) else {
            return Err(CODE_NOT_READY);
        };
        let id = self.desk.issue(&self.stats, req, sess(l, a), now)?;
        if self.render_ws(l, req, a, id, now).is_err() {
            let _ = self.desk.take(id);
            return Err(CODE_RENDER);
        }
        self.weigh(l, verb_weight(req.verb, self.lanes[l].product), now);
        if matches!(req.verb, REQ_PLACE | REQ_CANCEL | REQ_MODIFY) {
            self.stats.note_action_sent();
        }
        Ok(())
    }

    /// `req` rendered into session `a`'s send window as WS API request `id`.
    fn render_ws(&mut self, l: usize, r: WsReq, a: usize, id: u64, now: u64) -> Result<(), core_net::WsErr> {
        let ts = self.clock.venue_ms(now);
        let st = self.lanes[l].st;
        if r.ix == IX_NONE && r.verb != REQ_ACCOUNT {
            // Only the REST route posts an order-less verb (a stray's
            // cancel by its venue id); the WS API renders its own.
            debug_assert!(false, "an order-less WS API post");
            return Err(core_net::WsErr::Overflow);
        }
        match r.verb {
            REQ_PLACE | REQ_MODIFY => {
                let (e, x) = (self.oot.get(r.ix), self.oot.aux(r.ix));
                let modify = r.verb == REQ_MODIFY;
                let w = self.wire.row(e.row);
                let o = Place {
                    symbol: w.symbol(),
                    side: e.side,
                    maker: modify || e.kind == ORDER_KIND_MAKER,
                    reduce_only: !modify && x.place_flags & core_types::ORDER_FLAG_REDUCE_ONLY != 0,
                    qty_1e6: if modify { x.pending_qty_1e6 } else { e.qty_1e6 },
                    qty_dec: w.qty_dec,
                    px_1e6: if modify { x.pending_px_1e6 } else { e.px_1e6 },
                    px_dec: w.px_dec,
                    cid: Part::Cid(&self.prefix, e.slot, e.cid_oid),
                    ts_ms: ts,
                };
                if modify {
                    queue_modify(&mut self.lanes[l].ws[a], id, &st, &o)
                } else {
                    queue_place(&mut self.lanes[l].ws[a], id, &st, &o)
                }
            }
            REQ_CANCEL | REQ_STATUS => {
                let e = self.oot.get(r.ix);
                let sym = self.wire.row(e.row).symbol();
                let cid = Part::Cid(&self.prefix, e.slot, e.cid_oid);
                if r.verb == REQ_CANCEL {
                    queue_cancel(&mut self.lanes[l].ws[a], id, &st, sym, cid, ts)
                } else {
                    queue_status(&mut self.lanes[l].ws[a], id, &st, sym, cid, ts)
                }
            }
            _ => queue_account(&mut self.lanes[l].ws[a], id, &st, self.lanes[l].product, ts),
        }
    }

    /// Every lane's REST route pumped ([`Self::pump_route`]).
    fn pump_routes(&mut self, now: u64) {
        let mut l = 0;
        while l < self.lanes.len() {
            self.pump_route(l, now);
            l += 1;
        }
    }

    /// **The REST route's pump** (BX7, C5): while its connection is free,
    /// the next request — dropped if its order ended while it waited —
    /// rendered from the table, signed and started. Nothing while quiet.
    fn pump_route(&mut self, l: usize, now: u64) {
        while now >= self.quiet_until_ns {
            let r = match self.lanes[l].route.as_ref() {
                Some(rt) if rt.wire.is_none() => match rt.queue.front() {
                    Some(q) => q.req,
                    None => return,
                },
                _ => return,
            };
            if r.ix != IX_NONE && !self.for_open(&r) {
                if let Some(rt) = self.lanes[l].route.as_mut() {
                    rt.queue.advance();
                }
                continue;
            }
            match self.start_rest_order(l, r, now) {
                RestStart::Started => {
                    if let Some(rt) = self.lanes[l].route.as_mut() {
                        rt.queue.advance();
                        rt.wire = Some(RestReq { req: r, sent_ns: now });
                    }
                    self.weigh(l, verb_weight(r.verb, self.lanes[l].product), now);
                    if matches!(r.verb, REQ_PLACE | REQ_CANCEL | REQ_MODIFY) {
                        self.stats.note_action_sent();
                    }
                    return;
                }
                RestStart::Later => return,
                RestStart::Down => {
                    self.route_down(l, now);
                    return;
                }
                RestStart::Unrendered => {
                    if let Some(rt) = self.lanes[l].route.as_mut() {
                        rt.queue.advance();
                    }
                    self.unsent(r, CODE_RENDER, now);
                }
            }
        }
    }

    /// BX9: option row `row` is past its expiry (by the venue's clock).
    #[inline]
    fn expired(&self, row: u16, now: u64) -> bool {
        let e = self.wire.opt(row).expiry_ms;
        e > 0 && self.clock.venue_ms(now) >= e as u64
    }

    /// Render, sign and start `r` on the REST route's connection — an
    /// options lane's in eapi's words, with its own signer (BX9).
    fn start_rest_order(&mut self, l: usize, r: WsReq, now: u64) -> RestStart {
        let ts = self.clock.venue_ms(now);
        let st = self.lanes[l].st;
        let opt = self.lanes[l].product == PRODUCT_OPTIONS;
        let Some(rt) = self.lanes[l].route.as_mut() else {
            return RestStart::Later;
        };
        if opt && matches!(r.verb, REQ_CANCEL | REQ_MODIFY) {
            // IoC-only (BX-17): the arm builds neither, nor does the gateway.
            debug_assert!(false, "an options cancel or modify");
            return RestStart::Unrendered;
        }
        let t = match r.verb {
            REQ_PLACE => R_PLACE,
            REQ_CANCEL => R_CANCEL,
            REQ_MODIFY => R_MODIFY,
            REQ_STATUS => R_STATUS,
            _ => R_ACCOUNT,
        };
        let win = rt.conn.window_mut(t);
        if win.is_empty() {
            // A dial is pending: the window comes back next pass.
            return RestStart::Later;
        }
        let mut w = QueryWriter::new(win);
        match r.verb {
            REQ_PLACE | REQ_MODIFY => {
                let (e, x) = (self.oot.get(r.ix), self.oot.aux(r.ix));
                let modify = r.verb == REQ_MODIFY;
                let row = self.wire.row(e.row);
                let o = Place {
                    symbol: row.symbol(),
                    side: e.side,
                    maker: modify || e.kind == ORDER_KIND_MAKER,
                    reduce_only: !modify && x.place_flags & core_types::ORDER_FLAG_REDUCE_ONLY != 0,
                    qty_1e6: if modify { x.pending_qty_1e6 } else { e.qty_1e6 },
                    qty_dec: row.qty_dec,
                    px_1e6: if modify { x.pending_px_1e6 } else { e.px_1e6 },
                    px_dec: row.px_dec,
                    cid: Part::Cid(&self.prefix, e.slot, e.cid_oid),
                    ts_ms: ts,
                };
                if modify {
                    render_modify(&mut w, &st, &o);
                } else if opt {
                    render_opt_place(&mut w, &st, &o);
                } else {
                    render_place(&mut w, &st, &o);
                }
            }
            REQ_STATUS if opt => {
                let e = self.oot.get(r.ix);
                let sym = self.wire.row(e.row).symbol();
                render_opt_by_cid(&mut w, &st, sym, Part::Cid(&self.prefix, e.slot, e.cid_oid), ts);
            }
            REQ_CANCEL if r.ix == IX_NONE => {
                // A stray: `gen` carries its row, `oid` the venue's id.
                let sym = self.wire.row(r.gen as u16).symbol();
                render_by_order_id(&mut w, &st, sym, r.oid, ts);
            }
            REQ_CANCEL | REQ_STATUS => {
                let e = self.oot.get(r.ix);
                let sym = self.wire.row(e.row).symbol();
                render_by_cid(&mut w, &st, sym, Part::Cid(&self.prefix, e.slot, e.cid_oid), ts);
            }
            _ => render_account(&mut w, &st, ts),
        }
        let signed = match (opt, self.opt_signer.as_ref()) {
            (false, _) => w.sign_with(&self.signer),
            (true, Some(sg)) => w.sign_with(sg),
            // The boot refuses options with no signer: unreachable.
            (true, None) => return RestStart::Unrendered,
        };
        let Ok(len) = signed else {
            return RestStart::Unrendered;
        };
        match rt.conn.start(t, len, self.poll.registry(), now) {
            Ok(()) => RestStart::Started,
            Err(_) => {
                // The connection cannot send (a failed dial): the route is
                // down until an answer; the request waits at the front.
                rt.up = false;
                RestStart::Down
            }
        }
    }

    /// A request that never left (refused by the render, or its route went
    /// down before its turn): a place is refused (it never reached the
    /// venue), a modify aborted, a cancel owed again, a read failed.
    fn unsent(&mut self, r: WsReq, code: i32, now: u64) {
        if r.verb == REQ_ACCOUNT {
            if self.recon == ReconStep::Account {
                self.recon_failed();
            }
            return;
        }
        if !self.for_open(&r) {
            return;
        }
        match r.verb {
            REQ_PLACE => self.end_order(r.ix, EVT_REJECT, RETIRED_REJECTED, code, now),
            REQ_MODIFY => self.modify_aborted(r.ix, code, now),
            REQ_CANCEL => {
                let m = self.oot.get_mut(r.ix);
                let owe = if m.flags & OF_TTL != 0 { OWE_TTL } else { OWE_MEMBER };
                m.flags &= !(OF_CANCEL_SENT | OF_TTL);
                self.owe_cancel(r.ix, owe, now + CANCEL_RETRY_NS);
            }
            _ => {}
        }
    }

    /// The REST route's connection progressed: an answer (judged as the WS
    /// API's), or the request on the wire lost — then everything still
    /// queued never left (`unsent`) and the route is down until an answer.
    fn on_route_progress(&mut self, l: usize, p: Progress, now: u64) {
        let (status, span) = match p {
            Progress::Waiting => return,
            Progress::Done { status, body } => (status, body),
            Progress::Failed(_) => {
                let lost = match self.lanes[l].route.as_mut() {
                    Some(rt) => {
                        rt.up = false;
                        rt.wire.take()
                    }
                    None => return,
                };
                self.route_down(l, now);
                self.t[crate::arm::GW_CONNECT_FAILURES] += 1;
                if let Some(q) = lost {
                    self.stats.note_post_timeout();
                    self.on_request_lost(q.req, now);
                }
                while let Some(q) = self.lanes[l].route.as_mut().and_then(|rt| rt.queue.pop()) {
                    self.unsent(q.req, CODE_NOT_READY, now);
                }
                return;
            }
        };
        let Some(q) = self.lanes[l].route.as_mut().and_then(|rt| {
            rt.up = true;
            rt.rx_ns = now;
            rt.wire.take()
        }) else {
            return;
        };
        self.lanes[l].ord_down_ns = 0;
        self.stats.note_reply();
        let mut a = WsAnswer::default();
        let scanned = scan_rest_answer(status, self.frame(Frame::Rest(l, span.start, span.end)), &mut a);
        match scanned {
            Ok(()) => self.answered(q.req, q.sent_ns, Frame::Rest(l, span.start, span.end), &a, now),
            Err(e) => {
                // An answer we cannot read says nothing of the order: in
                // doubt (BX-11), and a halt observation (BX-15). A 418 or a
                // 429 still says the IP is banned or over its budget,
                // whatever its body (a CDN's page, a code named twice): the
                // quiet and the lock or the budget, as the jobs judge it
                // (`classify(0, http)`; the risk review's round-3 finding 2).
                self.scan_failed(SCAN_REST, e, now);
                if status == 418 || status == 429 {
                    let until = scan_ban_until(self.frame(Frame::Rest(l, span.start, span.end)));
                    self.venue_code(0, status, until, now);
                }
                self.on_request_lost(q.req, now);
            }
        }
        self.pump_route(l, now);
    }

    /// Lane `l`'s REST route went down: since now, unless it already was.
    #[inline]
    fn route_down(&mut self, l: usize, now: u64) {
        let ln = &mut self.lanes[l];
        if ln.ord_down_ns == 0 {
            ln.ord_down_ns = now;
        }
    }

    /// An answer's bytes ([`frame_of`]).
    #[inline]
    fn frame(&self, f: Frame) -> &[u8] {
        frame_of(&self.lanes, f)
    }

    /// Count lane `l`'s request of weight `w` in the IP's one-minute window
    /// (BX7: the run-time governor) — never a spot lane's: spot's
    /// REQUEST_WEIGHT is its own pool (BX8, `gov::arm_weight_per_min`); an
    /// options lane's in eapi's own window (BX9).
    #[inline]
    fn weigh(&mut self, l: usize, w: Weighed, now: u64) {
        match self.lanes[l].product {
            PRODUCT_SPOT => {}
            PRODUCT_OPTIONS => self.opt_weight.add(now, request_weight(w)),
            _ => self.weight.add(now, request_weight(w)),
        }
    }

    /// The weight spent reached [`WEIGHT_THROTTLE_PCT`] of the arm's
    /// share — in the IP's futures pool or eapi's (BX9) — a reconciliation
    /// asked for before its cadence waits (the one read the boot's weight
    /// model does not count).
    #[inline]
    fn throttled(&self) -> bool {
        self.weight.at_least(ARM_WEIGHT_MAX_PER_MIN, WEIGHT_THROTTLE_PCT)
            || self.opt_weight.at_least(OPT_ARM_WEIGHT_MAX_PER_MIN, WEIGHT_THROTTLE_PCT)
    }

    /// BX9 (Q-5): eapi's window can take one more of the day's reads
    /// (`userTrades`, weight 5) and still a whole cycle within the share —
    /// the day is paced, never the cycle the boot's model counts.
    #[inline]
    fn opt_day_room(&mut self, now: u64) -> bool {
        self.opt_weight.advance(now);
        self.opt_weight.sum() + u64::from(request_weight(Weighed::UserTrades)) + W_OPT_RECON <= OPT_ARM_WEIGHT_MAX_PER_MIN
    }

    /// A command's push time, for the burst's `push_to_write`.
    #[inline]
    fn burst_push(&mut self, pushed_ns: u64) {
        if self.burst_n < CMD_BURST {
            self.burst_pushed[self.burst_n] = pushed_ns;
            self.burst_n += 1;
        }
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
            VERB_SHUTDOWN => {
                // Leave once the sweep the arm asked for alongside ends —
                // or this one, if none runs.
                if self.sweep.active {
                    self.sweep.shutdown = true;
                } else {
                    self.start_sweep(now, true);
                }
            }
            _ => {}
        }
        self.burst_push(c.pushed_ns);
    }

    /// Refuse a place locally: the arm hears a REJECT with a local code.
    fn refuse_place(&mut self, c: &BnCmd, code: i32, now: u64) {
        let mut e = BnEvt::order(EVT_REJECT, now, c.client_oid, c.row, c.slot);
        e.code = code;
        e.why = RETIRED_REJECTED;
        e.side = c.side;
        e.product = if (c.row as usize) < self.wire.len() { self.wire.row(c.row).product } else { PRODUCT_USDM };
        e.flags = (c.kind != ORDER_KIND_MAKER) as u8 * EVT_F_IOC;
        self.emit(&e);
        self.note(J_REJECT, now, c.client_oid, 0, 0, 0, 0, code, c.row, c.slot);
    }

    fn on_place(&mut self, c: &BnCmd, now: u64) {
        if (c.row as usize) >= self.wire.len() {
            return self.refuse_place(c, CODE_NOT_READY, now);
        }
        let l = self.lane_of_row(c.row);
        if !self.ready_to_send(l, now) {
            let code = self.not_ready_code(now);
            return self.refuse_place(c, code, now);
        }
        // BX9: an option row past its expiry trades no more — its leg waits
        // for the settlement (module docs).
        if self.lanes[l].product == PRODUCT_OPTIONS && self.expired(c.row, now) {
            return self.refuse_place(c, CODE_EXPIRED, now);
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
        // XG-3: a maker whose TTL cannot be timed out is not sent.
        if c.ttl_deadline_ns != 0 && !self.arm_order(ix, c.ttl_deadline_ns) {
            self.oot.remove(ix);
            return self.refuse_place(c, CODE_TIMER_FULL, now);
        }
        self.oot.aux_mut(ix).place_flags = c.flags;
        let gen = self.oot.aux(ix).gen;
        if let Err(code) = self.post(l, WsReq::on(REQ_PLACE, ix, gen, c.client_oid), now) {
            self.disarm_order(ix);
            self.oot.remove(ix);
            return self.refuse_place(c, code, now);
        }
        let maker = c.kind == ORDER_KIND_MAKER;
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
    /// whose it is, `OWE_*`) — retried from the deadline queue until the
    /// session takes it. A cancel in flight is not repeated.
    fn cancel_or_owe(&mut self, ix: u16, now: u64, owe: u8) {
        if self.oot.get(ix).flags & OF_CANCEL_SENT != 0 {
            return;
        }
        if !self.send_cancel(ix, now, owe == OWE_TTL) {
            self.owe_cancel(ix, owe, now + CANCEL_RETRY_NS);
        }
    }

    /// Owe the cancel of `ix` (`OWE_*`), retried at `at_ns` from the
    /// deadline queue — or sooner, if its deadline (a TTL) falls earlier.
    fn owe_cancel(&mut self, ix: u16, owe: u8, at_ns: u64) {
        // Counted when the order becomes owed, not on every retry.
        self.t[crate::arm::GW_CANCELS_OWED] += (self.oot.get(ix).owe & (OWE_MEMBER | OWE_TTL) == 0) as u64;
        self.oot.get_mut(ix).owe |= owe;
        if self.order_tid[ix as usize].is_some() && self.order_at[ix as usize] <= at_ns {
            return;
        }
        let _ = self.arm_order(ix, at_ns);
    }

    /// Send the cancel of the open order in slot `ix` (a member's, the
    /// TTL's or a sweep's) now. `false`: not sent — one is in flight, or
    /// the session cannot take it now.
    fn send_cancel(&mut self, ix: u16, now: u64, ttl: bool) -> bool {
        let (flags, oid, row) = {
            let e = self.oot.get(ix);
            (e.flags, e.cid_oid, e.row)
        };
        let l = self.lane_of_row(row);
        if flags & OF_CANCEL_SENT != 0 || !self.ready_to_send(l, now) {
            return false;
        }
        let gen = self.oot.aux(ix).gen;
        if self.post(l, WsReq::on(REQ_CANCEL, ix, gen, oid), now).is_err() {
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
        let (flags, oid, row) = {
            let e = self.oot.get(ix);
            (e.flags, e.cid_oid, e.row)
        };
        if flags & (OF_MODIFY_SENT | OF_CANCEL_SENT) != 0 {
            return self.modify_failed(c, CODE_MODIFY_BUSY, now);
        }
        let l = self.lane_of_row(row);
        if !self.ready_to_send(l, now) {
            let code = self.not_ready_code(now);
            return self.modify_failed(c, code, now);
        }
        if !self.oot.add_pending(ix, c.client_oid, c.px_1e6, c.qty_1e6, now) {
            return self.modify_failed(c, CODE_DUPLICATE, now);
        }
        let gen = self.oot.aux(ix).gen;
        if let Err(code) = self.post(l, WsReq::on(REQ_MODIFY, ix, gen, oid), now) {
            self.oot.abort_rename(ix);
            self.modify_failed(c, code, now);
        }
    }

    // ---------------------------------------------------------------------
    // The sweep (plan §3.11 switch 1; O-BX8, O-BX9: never a flatten)
    // ---------------------------------------------------------------------

    /// Start (or restart: a newer request) the sweep; the word says it is
    /// being worked. A shutdown sweep stays one through a restart.
    fn start_sweep(&mut self, now: u64, shutdown: bool) {
        self.sweep = Sweep {
            active: true,
            round: 0,
            confirm_at_ns: 0,
            cancels: 0,
            shutdown: shutdown || (self.sweep.active && self.sweep.shutdown),
        };
        self.stats.set_sweep(self.sweep_seq, SWEEP_WORKING);
        self.retire_queued(now);
        self.sweep_round(now);
    }

    /// The REST order route's places and modifies still queued — admitted,
    /// never sent — retired before the sweep's cancels queue behind them
    /// (BX7): the WS API would have sent them at admission; here they would
    /// leave first. A place is refused, a modify aborted ([`CODE_SWEPT`]);
    /// cancels and reads keep their order.
    fn retire_queued(&mut self, now: u64) {
        let mut l = 0;
        while l < self.lanes.len() {
            let n = self.lanes[l].route.as_ref().map_or(0, |rt| rt.queue.len());
            let mut i = 0;
            while i < n {
                let Some(q) = self.lanes[l].route.as_mut().and_then(|rt| rt.queue.pop()) else {
                    break;
                };
                if matches!(q.req.verb, REQ_PLACE | REQ_MODIFY) {
                    self.unsent(q.req, CODE_SWEPT, now);
                } else if let Some(rt) = self.lanes[l].route.as_mut() {
                    // Just popped: there is room behind the rest.
                    let _ = rt.queue.push(q);
                }
                i += 1;
            }
            l += 1;
        }
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
        // cancels that keep failing strand the makers — while the row's
        // session is down, from the second round on, and from the first
        // after a sweep stranded, each row with a maker of ours resting is
        // cancelled over REST (unless the gateway is quiet: the dead-man
        // then does it).
        let every_row = self.sweep.round >= 1 || self.sweep_streak > 0;
        let mut r = 0;
        while r < self.rows.len() {
            if self.rows[r].makers > 0
                && (every_row || !self.ready_to_send(self.lane_of_row(r as u16), now))
                && self.rest_cancel_all(r as u16, now)
            {
                self.sweep.cancels += 1;
            }
            r += 1;
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
        if now < self.quiet_until_ns || self.rows[r].cancel_all_queued || !self.queue_job(self.lane_of_row(row), JobKind::CancelAll, row) {
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
        // The outcome the router waits on (XG-1), then the report.
        let word = if flag & EVT_F_SWEEP_DONE != 0 { SWEEP_CLEAR } else { SWEEP_STRANDED };
        self.stats.set_sweep(self.sweep_seq, word);
        self.stats.note_sweep_cancels(self.sweep.cancels.max(0) as u64);
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
        let mut l = 0;
        while l < self.lanes.len() {
            let mut s = 0;
            while s < 2 {
                if self.lanes[l].ws[s].wants_flush() && self.lanes[l].ws[s].flush(self.poll.registry()).is_err() {
                    self.ws_failed(l, s, core_time::now_ns());
                }
                s += 1;
            }
            if self.lanes[l].us.wants_flush() && self.lanes[l].us.flush(self.poll.registry()).is_err() {
                self.us_failed(l, core_time::now_ns());
            }
            let failed = match self.lanes[l].risk.as_mut() {
                Some(r) => r.ws.wants_flush() && r.ws.flush(self.poll.registry()).is_err(),
                None => false,
            };
            if failed {
                self.risk_failed(l, core_time::now_ns());
            }
            l += 1;
        }
    }

    fn dispatch_events(&mut self, now: u64) {
        // The batch is moved out for the dispatch and back after it: a
        // zero-capacity `Events` owns no buffer, so this allocates nothing.
        let events = core::mem::replace(&mut self.events, Events::with_capacity(0));
        for ev in events.iter() {
            let t = ev.token();
            if t == TOKEN_SAPI {
                if let Some(s) = self.sapi.as_mut() {
                    let p = s.on_event(ev, self.poll.registry());
                    self.on_rest_progress(0, p, now, true);
                }
                continue;
            }
            // Lane `l`'s connection `k` (`lane_token`).
            let (l, k) = (t.0 / LANE_TOKENS, t.0 % LANE_TOKENS);
            if l >= self.lanes.len() {
                continue;
            }
            match k {
                TK_WS0 | TK_WS1 => {
                    let s = k - TK_WS0;
                    let p = self.lanes[l].ws[s].on_event(ev, self.poll.registry(), now);
                    self.on_ws_progress(l, s, p, now);
                }
                TK_US => {
                    let p = self.lanes[l].us.on_event(ev, self.poll.registry(), now);
                    self.on_us_progress(l, p, now);
                }
                TK_REST => {
                    let p = self.lanes[l].rest.on_event(ev, self.poll.registry());
                    self.on_rest_progress(l, p, now, false);
                }
                TK_ORD => {
                    let p = match self.lanes[l].route.as_mut() {
                        Some(rt) => rt.conn.on_event(ev, self.poll.registry()),
                        None => Progress::Waiting,
                    };
                    self.on_route_progress(l, p, now);
                }
                TK_RISK => {
                    let p = match self.lanes[l].risk.as_mut() {
                        Some(r) => r.ws.on_event(ev, self.poll.registry(), now),
                        None => WsProgress::Waiting,
                    };
                    self.on_risk_progress(l, p, now);
                }
                _ => {}
            }
        }
        self.events = events;
    }

    /// Lane `l`'s order session `s`'s socket progressed.
    fn on_ws_progress(&mut self, l: usize, s: usize, p: WsProgress, now: u64) {
        match p {
            WsProgress::Waiting => {}
            WsProgress::Opened => {
                if now < self.quiet_until_ns {
                    // N3: a dial that completes inside a quiet period sends
                    // nothing — dropped, and dialled again after it (the
                    // machine dials only outside one).
                    self.lanes[l].ws[s].close();
                    self.lanes[l].mbb.on_down(s as u8, now);
                    return;
                }
                self.lanes[l].opened_ns[s] = now;
                self.lanes[l].rx_ns[s] = now;
                let again = self.lanes[l].ws_opens > 0;
                self.t[crate::arm::GW_RECONNECTS] += again as u64;
                if again {
                    self.stats.note_reconnect();
                }
                self.lanes[l].ws_opens += 1;
                // session.logon: once per connection, the one signer. Its
                // answer makes the slot live (`went_live`); none within
                // the dial's budget retires it.
                let Some(ed) = self.signer.ed25519() else {
                    // An HMAC key never dials the WS API (the boot refuses
                    // it): nothing can log on.
                    debug_assert!(false, "a WS API session without an Ed25519 key");
                    return;
                };
                if let Ok(id) = self.desk.issue(&self.stats, WsReq::on(REQ_LOGON, IX_NONE, 0, 0), sess(l, s), now) {
                    // Field by field (the signer is lent): `weigh`'s law.
                    if self.lanes[l].product != PRODUCT_SPOT {
                        self.weight.add(now, request_weight(Weighed::Logon));
                    }
                    let key = self.api_key.as_bytes();
                    if queue_logon(&mut self.lanes[l].ws[s], id, key, self.clock.venue_ms(now), ed).is_err() {
                        let _ = self.desk.take(id);
                    }
                }
                self.drain_ws(l, s, now);
            }
            WsProgress::Readable => self.drain_ws(l, s, now),
            WsProgress::Failed(_) => self.ws_failed(l, s, now),
        }
    }

    fn drain_ws(&mut self, l: usize, s: usize, now: u64) {
        loop {
            let span = match self.lanes[l].ws[s].next_frame() {
                WsNext::Idle => return,
                WsNext::Text(sp) | WsNext::Binary(sp) => sp,
                WsNext::Failed(_) => return self.ws_failed(l, s, now),
            };
            self.lanes[l].rx_ns[s] = now;
            let mut a = WsAnswer::default();
            let scanned = scan_answer(self.lanes[l].ws[s].payload(span), &mut a);
            match scanned {
                Ok(()) if a.user => self.spot_event(l, s, span, a.event.at as usize, now),
                Ok(()) => self.on_answer(l, s, &a, span, now),
                Err(e) => self.scan_failed(SCAN_WSAPI, e, now),
            }
        }
    }

    /// **A spot user event** (BX8): the `event` object at `at` in session
    /// `(l, s)`'s frame — read in place ([`scan_spot_event`]), its row, id
    /// and commission resolved while the frame is borrowed, then booked as
    /// a futures update is. On a futures session it is no shape of theirs
    /// (a scan failure); the subscription's end drops the session.
    fn spot_event(&mut self, l: usize, s: usize, span: core_net::PayloadSpan, at: usize, now: u64) {
        if self.lanes[l].product != PRODUCT_SPOT {
            return self.scan_failed(SCAN_USER, ScanErr::Malformed, now);
        }
        let mut ev = UserEvent::default();
        let resolved = {
            let frame = self.lanes[l].ws[s].payload(span);
            match scan_spot_event(frame, at, &mut ev) {
                Err(e) => Err(e),
                Ok(()) if ev.kind != UE_ORDER => Ok((ROW_NONE, CidClass::Foreign, FEE_ASSET_NONE, ROW_NONE)),
                Ok(()) => {
                    let row = self.wire.find(PRODUCT_SPOT, ev.symbol.get(frame));
                    // BX-8 on spot: no liquidation, ADL or settlement exists
                    // there — an id spelling one is someone else's order.
                    let class = match self.prefix.classify(ev.cid.get(frame)) {
                        CidClass::Liquidation | CidClass::Adl | CidClass::Settlement => CidClass::Foreign,
                        c => c,
                    };
                    let asset = if row == ROW_NONE { FEE_ASSET_NONE } else { fee_asset(self.wire.row(row), ev.fee_asset.get(frame)) };
                    Ok((row, class, asset, self.spot_row_of(ev.fee_asset.get(frame))))
                }
            }
        };
        match resolved {
            Err(e) => self.scan_failed(SCAN_USER, e, now),
            Ok(_) if ev.kind == UE_STREAM_END => self.ws_failed(l, s, now),
            Ok((row, class, asset, fee_row)) => self.on_user_event(l, &ev, row, class, asset, fee_row, now),
        }
    }

    /// The armed spot row whose base asset is `asset` (BX8), or
    /// [`ROW_NONE`].
    fn spot_row_of(&self, asset: &[u8]) -> u16 {
        let mut k = 0;
        while k < self.n_spot {
            if self.spot_assets[k].get() == asset {
                return self.spot_rows[k];
            }
            k += 1;
        }
        ROW_NONE
    }

    fn on_us_progress(&mut self, l: usize, p: WsProgress, now: u64) {
        match p {
            WsProgress::Waiting => {}
            WsProgress::Opened => {
                self.lanes[l].us_backoff.reset();
                self.t[crate::arm::GW_RECONNECTS] += (self.lanes[l].us.dials() > 1) as u64;
                if self.phase == Phase::Ready {
                    // What the venue sent while the stream was down is gone
                    // (Binance does not replay): every open order of the
                    // lane is in doubt, and the books are checked now (B2).
                    self.doubt_lane(l);
                    self.recon_next_ns = now;
                }
                self.drain_us(l, now);
            }
            WsProgress::Readable => self.drain_us(l, now),
            WsProgress::Failed(_) => self.us_failed(l, now),
        }
    }

    fn drain_us(&mut self, l: usize, now: u64) {
        loop {
            let span = match self.lanes[l].us.next_frame() {
                WsNext::Idle => return,
                WsNext::Text(s) | WsNext::Binary(s) => s,
                WsNext::Failed(_) => return self.us_failed(l, now),
            };
            self.lanes[l].user.last_rx_ns = now;
            let product = self.lanes[l].product;
            let mut ev = UserEvent::default();
            let frame = self.lanes[l].us.payload(span);
            match scan_user_event(frame, &mut ev) {
                Ok(()) => {
                    // A Portfolio Margin event names its product (`fs`); on
                    // a classic stream an event naming one is no shape of its
                    // own — a scan failure, never routed by it (the risk
                    // review's N6).
                    if ev.fs != 0 && !self.lanes[l].pm {
                        self.scan_failed(SCAN_USER, ScanErr::BadField, now);
                        continue;
                    }
                    // ...and on Portfolio Margin's one stream, an order,
                    // account or configuration event naming no product is
                    // no shape of PM's: a scan failure, never read as the
                    // first lane's (the risk review's round-3 finding 6).
                    let product_event = ev.kind == UE_ORDER || ev.kind == UE_TRADE_LITE || ev.kind == UE_ACCOUNT || ev.kind == UE_CONFIG;
                    if ev.fs == 0 && self.lanes[l].pm && product_event {
                        self.scan_failed(SCAN_USER, ScanErr::Missing, now);
                        continue;
                    }
                    // Resolve the row and the id while the frame is borrowed:
                    // a classic stream's events are its lane's.
                    let p = if ev.fs == 0 { product } else { ev.fs };
                    let named = ev.kind == UE_ORDER || ev.kind == UE_TRADE_LITE || ev.config & CONFIG_LEVERAGE != 0 || ev.isolated > 0;
                    let mut row = if named && p != u8::MAX {
                        self.wire.find(p, ev.symbol.get(frame))
                    } else {
                        ROW_NONE
                    };
                    // BX-19 (BX7): an `ACCOUNT_UPDATE` naming several positions
                    // not cross — the first owned one is the event's row, found
                    // in one pass by the scan's own reader ([`NotCross`]).
                    if ev.isolated > 1 && p != u8::MAX && (row == ROW_NONE || self.wire.row(row).flags & INST_OWNED == 0) {
                        let mut walk = NotCross::new(frame, &ev);
                        // The first is the event's own symbol, looked up above.
                        let _ = walk.next();
                        while let Some(s) = walk.next() {
                            row = self.wire.find(p, s.get(frame));
                            if row != ROW_NONE && self.wire.row(row).flags & INST_OWNED != 0 {
                                break;
                            }
                        }
                    }
                    let class = self.prefix.classify(ev.cid.get(frame));
                    // BX11: the commission's asset to its row (`J_FEE`).
                    let asset = if ev.kind == UE_ORDER && row != ROW_NONE {
                        fee_asset(self.wire.row(row), ev.fee_asset.get(frame))
                    } else {
                        FEE_ASSET_NONE
                    };
                    // BX9: an options commission is a charge whatever its
                    // sign (IoC: always a taker's; eapi's `fee` is negative
                    // where fapi's `n` is positive).
                    if product == PRODUCT_OPTIONS {
                        ev.commission_1e9 = ev.commission_1e9.saturating_abs();
                    }
                    self.on_user_event(l, &ev, row, class, asset, ROW_NONE, now);
                }
                Err(e) => self.scan_failed(SCAN_USER, e, now),
            }
        }
    }

    /// PM Pro's risk stream progressed (BX7, C3).
    fn on_risk_progress(&mut self, l: usize, p: WsProgress, now: u64) {
        match p {
            WsProgress::Waiting => {}
            WsProgress::Opened => {
                if let Some(r) = self.lanes[l].risk.as_mut() {
                    r.backoff.reset();
                }
                self.drain_risk(l, now);
            }
            WsProgress::Readable => self.drain_risk(l, now),
            WsProgress::Failed(_) => self.risk_failed(l, now),
        }
    }

    fn drain_risk(&mut self, l: usize, now: u64) {
        loop {
            let mut ev = UserEvent::default();
            let scanned = {
                let Some(r) = self.lanes[l].risk.as_mut() else {
                    return;
                };
                let span = match r.ws.next_frame() {
                    WsNext::Idle => return,
                    WsNext::Text(s) | WsNext::Binary(s) => s,
                    WsNext::Failed(_) => return self.risk_failed(l, now),
                };
                scan_user_event(r.ws.payload(span), &mut ev)
            };
            match scanned {
                // Only the risk level is acted on; `PM_PRO_ACCOUNT_UPDATE`
                // is left to the cycle's account read.
                Ok(()) if ev.kind == UE_RISK => self.on_risk_level(l, &ev, now),
                Ok(()) => {}
                Err(e) => self.scan_failed(SCAN_USER, e, now),
            }
        }
    }

    fn risk_failed(&mut self, l: usize, now: u64) {
        if let Some(r) = self.lanes[l].risk.as_mut() {
            if !r.ws.is_down() {
                r.ws.close();
            }
            r.retry_ns = now + r.backoff.next_delay_ns();
        }
        self.t[crate::arm::GW_CONNECT_FAILURES] += 1;
    }

    fn scan_failed(&mut self, which: i32, _e: ScanErr, now: u64) {
        let mut e = BnEvt::new(EVT_SCAN_FAIL, now);
        e.code = which;
        self.emit(&e);
    }

    /// Lane `l`'s order session `s` died (or was judged dead, S2): every
    /// request in flight on it is in doubt (BX-11); the machine dials again
    /// on its backoff. The other session, if live, carries on.
    fn ws_failed(&mut self, l: usize, s: usize, now: u64) {
        let w = &mut self.lanes[l].ws[s];
        if w.is_open() || !w.is_down() {
            w.close();
        }
        let c = s as u8;
        if self.lanes[l].mbb.state(c) == SlotState::Down {
            // Already handled (a late error on a closed socket).
            return;
        }
        let was_active = self.lanes[l].mbb.active() == Some(c);
        self.lanes[l].mbb.on_down(c, now);
        if was_active || (self.lanes[l].mbb.active().is_none() && self.lanes[l].ord_down_ns == 0) {
            self.lanes[l].ord_down_ns = now;
        }
        self.t[crate::arm::GW_CONNECT_FAILURES] += 1;
        self.lose_requests(sess(l, s), now);
    }

    /// Every request in flight on order session `c` ([`sess`]) will never
    /// be answered.
    fn lose_requests(&mut self, c: usize, now: u64) {
        let mut k = 0;
        while k < WS_IDS {
            if let Some(id) = self.desk.pending.id_at(k) {
                if self.desk.pending.get(id).is_some_and(|r| r.conn as usize == c) {
                    if let Some((r, _)) = self.desk.take(id) {
                        self.on_request_lost(r.req, now);
                    }
                }
            }
            k += 1;
        }
    }

    /// Session `s`'s logon was answered: it takes the posts. Up after a gap
    /// with no live session, what the last one left unanswered is resolved
    /// by status (BX-11); a make-before-break swap lost nothing — the old
    /// session drains, its answers still land.
    fn went_live(&mut self, l: usize, s: usize, now: u64) {
        let recovery = self.lanes[l].mbb.active().is_none();
        self.lanes[l].mbb.on_live(s as u8, now);
        self.lanes[l].ord_down_ns = 0;
        if recovery && self.phase == Phase::Ready {
            self.doubt_lane(l);
            // BX8: spot's user data rode the lost session — what the venue
            // sent meanwhile is gone (Binance does not replay): the books
            // are checked now, as a reopened futures stream's are (B2).
            if self.lanes[l].product == PRODUCT_SPOT {
                self.recon_next_ns = now;
            }
        }
    }

    /// **Spot's subscription** (BX8): a session logged on asks for the
    /// account's user data on itself; only its 200 makes the session live
    /// — a session that could place but not see its fills never is. A
    /// subscription that cannot be asked drops the session.
    fn subscribe(&mut self, l: usize, s: usize, now: u64) {
        let Ok(id) = self.desk.issue(&self.stats, WsReq::on(REQ_SUBSCRIBE, IX_NONE, 0, 0), sess(l, s), now) else {
            return self.ws_failed(l, s, now);
        };
        if queue_subscribe(&mut self.lanes[l].ws[s], id).is_err() {
            let _ = self.desk.take(id);
            self.ws_failed(l, s, now);
        }
    }

    /// The order sessions' machine (XG-4): a drained replaced session is
    /// closed; dials and give-ups happen only outside a quiet period (N3)
    /// and once the boot reaches its sockets.
    fn drive_mbb(&mut self, l: usize, now: u64) {
        let mut s = 0;
        while s < 2 {
            if self.lanes[l].mbb.state(s as u8) == SlotState::Draining && self.desk.conn_live[sess(l, s)] == 0 {
                self.lanes[l].ws[s].close();
                self.lanes[l].mbb.on_drained(s as u8, now);
            }
            s += 1;
        }
        let connecting = matches!(self.phase, Phase::Connect | Phase::Recon | Phase::Day | Phase::Ready) && now >= self.quiet_until_ns;
        if !connecting {
            return;
        }
        match self.lanes[l].mbb.poll(now) {
            MbbAction::Idle => {}
            MbbAction::Dial(c) => {
                let w = &mut self.lanes[l].ws[c as usize];
                if !w.is_down() {
                    w.close();
                }
                if w.connect(self.poll.registry(), now).is_err() {
                    self.t[crate::arm::GW_CONNECT_FAILURES] += 1;
                    self.lanes[l].mbb.on_down(c, now);
                }
            }
            MbbAction::Retire(c) => {
                // A dial or a drain over its budget: what it carries is in
                // doubt.
                let dialing = self.lanes[l].mbb.state(c) == SlotState::Dialing;
                self.lanes[l].ws[c as usize].close();
                self.lanes[l].mbb.on_down(c, now);
                self.t[crate::arm::GW_CONNECT_FAILURES] += dialing as u64;
                self.lose_requests(sess(l, c as usize), now);
            }
        }
    }

    fn us_failed(&mut self, l: usize, now: u64) {
        if !self.lanes[l].us.is_down() {
            self.lanes[l].us.close();
        }
        if self.lanes[l].us_down_ns == 0 {
            self.lanes[l].us_down_ns = now;
        }
        self.t[crate::arm::GW_CONNECT_FAILURES] += 1;
        self.lanes[l].us_retry_ns = now + self.lanes[l].us_backoff.next_delay_ns();
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

    /// Every open order of lane `l` in doubt (its user stream reopened;
    /// its order session logged on again): `order.status` resolves each.
    fn doubt_lane(&mut self, l: usize) {
        let mut ix = 0u16;
        while (ix as usize) < OOT_CAP {
            if self.oot.is_open(ix) && self.lane_of_row(self.oot.get(ix).row) == l {
                self.doubt(ix);
            }
            ix += 1;
        }
    }

    // ---------------------------------------------------------------------
    // WS API answers
    // ---------------------------------------------------------------------

    /// An answer on lane `l`'s order session `s`.
    fn on_answer(&mut self, l: usize, s: usize, a: &WsAnswer, span: core_net::PayloadSpan, now: u64) {
        if a.shutdown {
            // serverShutdown: the session still works for now — replaced
            // make-before-break (a session that dies first is the cold
            // path).
            if self.lanes[l].mbb.active() == Some(s as u8) {
                self.lanes[l].mbb.request_replace();
            }
            return;
        }
        let Some((f, sent)) = self.desk.take(a.id) else {
            self.stats.note_reply_unmatched();
            return;
        };
        debug_assert_eq!(f.conn as usize, sess(l, s), "an answer on another session than its request's");
        self.stats.note_reply();
        // XG-4: a matched answer is data moved (the backoff resets only
        // after a healthy span of it).
        self.lanes[l].mbb.note_healthy(now);
        self.answered(f.req, sent, Frame::Ws(l, s, span), a, now);
    }

    /// **One answer to a request of ours** — from a WS API session or the
    /// REST route (BX7): the same judges for both.
    fn answered(&mut self, r: WsReq, sent: u64, fr: Frame, a: &WsAnswer, now: u64) {
        let rtt = now.saturating_sub(sent);
        match r.verb {
            REQ_PLACE | REQ_MODIFY => self.stats.post_rtt.record(rtt),
            REQ_CANCEL => self.stats.cancel_rtt.record(rtt),
            _ => {}
        }
        if a.status >= 400 || a.code != 0 {
            self.venue_code(a.code, a.status, a.retry_after_ms, now);
        }
        match r.verb {
            REQ_LOGON => {
                if let Frame::Ws(l, s, _) = fr {
                    if a.status != 200 {
                        self.ws_failed(l, s, now);
                    } else if self.lanes[l].product == PRODUCT_SPOT {
                        self.subscribe(l, s, now);
                    } else {
                        self.went_live(l, s, now);
                    }
                }
            }
            // Spot (BX8): the subscription makes the session live.
            REQ_SUBSCRIBE => {
                if let Frame::Ws(l, s, _) = fr {
                    if a.status == 200 && scan_subscribed(self.frame(fr), a.result) {
                        self.went_live(l, s, now);
                    } else {
                        self.ws_failed(l, s, now);
                    }
                }
            }
            REQ_PLACE if self.for_open(&r) => self.place_answered(r.ix, fr, a, now),
            REQ_CANCEL if self.for_open(&r) && a.status != 200 => self.cancel_refused(r.ix, a.code, now),
            REQ_MODIFY if self.for_open(&r) => self.modify_answered(r.ix, a, now),
            REQ_STATUS if self.for_open(&r) => self.status_answered(r.ix, a, now),
            REQ_ACCOUNT => self.account_answered(fr, a, now),
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
            CodeClass::Reject | CodeClass::Unknown | CodeClass::WouldTake | CodeClass::RowFatal | CodeClass::WriteFatal => {}
        }
    }

    fn place_answered(&mut self, ix: u16, fr: Frame, a: &WsAnswer, now: u64) {
        let (slot, oid, cur, row) = {
            let e = self.oot.get(ix);
            (e.slot, e.cid_oid, e.current_oid, e.row)
        };
        if a.status == 200 {
            // An ACK names the order: the venue's id, and ours echoed. A
            // 200 that does not is not an answer we know (BX-15): in
            // doubt, resolved by `order.status`.
            let echo = self.prefix.classify(a.cid.get(self.frame(fr)));
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
        self.disarm_order(ix);
        self.oot.get_mut(ix).owe = 0;
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
            if self.book_fill(Some(ix), None, row, slot, side, px, missing, J_FILL_UNSEEN, 0, ts, now, cur) {
                self.oot.get_mut(ix).flags |= OF_STATUS_BOOKED;
                // BX8: no report carried these trades' commissions.
                if self.rows[row as usize].spot_k != u8::MAX {
                    self.queue_order_trades(row, a.venue_oid, a.executed_1e6);
                }
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
            // A status naming the order refused (BX8, the risk review):
            // ended — queried again it would be forever.
            S_REJECTED => self.end_order(ix, EVT_REJECT, RETIRED_REJECTED, 0, now),
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
        x.product = self.wire.row(row).product;
        let icr = why == RETIRED_CANCELED_MEMBER && now.saturating_sub(placed_ns) < QTR_ICR_NS;
        x.flags = ((flags & OF_FILLED_ANY != 0) as u8 * EVT_F_HAD_FILL)
            | ((okind != ORDER_KIND_MAKER) as u8 * EVT_F_IOC)
            | (icr as u8 * EVT_F_ICR);
        self.emit(&x);
        let jk = if kind == EVT_REJECT { J_REJECT } else { J_ENDED };
        self.note(jk, now, cur, venue_oid, filled, qty, 0, if kind == EVT_REJECT { code } else { why as i32 }, row, slot);
        self.disarm_order(ix);
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

    /// `fee_row`: the armed spot row whose base asset the commission is
    /// charged in (BX8), else [`ROW_NONE`].
    #[allow(clippy::too_many_arguments)]
    fn on_user_event(&mut self, l: usize, ev: &UserEvent, row: u16, class: CidClass, asset: u8, fee_row: u16, now: u64) {
        match ev.kind {
            UE_ORDER | UE_TRADE_LITE => self.on_order_event(ev, row, class, asset, fee_row, now),
            UE_MARGIN_CALL => self.margin_call(l, now),
            UE_ACCOUNT => {
                // BX-19 (BX7): an owned row's position not cross-margined
                // — no event names a margin type change, so its first
                // position update does — is a lock.
                if ev.isolated > 0 && row != ROW_NONE && self.wire.row(row).flags & INST_OWNED != 0 {
                    self.margin_lock(CODE_MARGIN_TYPE, now);
                }
                // A margin re-read — the lane's account alone, no sooner
                // than ACCOUNT_REREAD_NS after its last read (every fill
                // pushes one, S5).
                let ln = &mut self.lanes[l];
                if ln.account_next_ns == 0 {
                    ln.account_next_ns = now.max(ln.account_read_ns + ACCOUNT_REREAD_NS);
                }
            }
            UE_RISK => self.on_risk_level(l, ev, now),
            // BX7 (carried): the account reconfigured under the arm — an
            // owned row's leverage, or the multi-assets mode turned on
            // where single-asset was asserted (USDⓈ-M classic) — is a
            // `VenueLock`; the restart re-asserts. A leverage change on a
            // symbol the arm does not own (unbound, or a shared account's
            // other rows) changes nothing of ours. One frame may say both.
            UE_CONFIG => {
                let multi = ev.config & CONFIG_MULTI_ASSETS_ON != 0 && self.lanes[l].product == PRODUCT_USDM && !self.lanes[l].unified;
                let owned = row != ROW_NONE && self.wire.row(row).flags & INST_OWNED != 0;
                if (ev.config & CONFIG_LEVERAGE != 0 && owned) || multi {
                    self.venue_lock(CODE_CONFIG, now);
                }
            }
            UE_KEY_EXPIRED => {
                self.lanes[l].user.expire();
                self.lanes[l].us.close();
                if self.lanes[l].us_down_ns == 0 {
                    self.lanes[l].us_down_ns = now;
                }
                let _ = self.queue_job(l, JobKind::ListenNew, 0);
            }
            // A Portfolio Margin borrow (`liabilityChange`): the unified
            // account's next read carries it.
            UE_LIABILITY => {}
            // A kind the arm does not act on: counted, never booked
            // (BX-15).
            _ => self.t[crate::arm::GW_EVENTS_OTHER] += 1,
        }
    }

    /// A margin call on lane `l` (BX-20): its product's last ratio flagged,
    /// and the lane's account read now, not at the next cycle.
    fn margin_call(&mut self, l: usize, now: u64) {
        let mut e = BnEvt::new(EVT_MARGIN, now);
        e.product = self.lanes[l].product;
        e.a = self.lanes[l].ratio_1e6;
        e.b = self.equity_1e6;
        e.c = self.anchor_seen();
        e.flags = EVT_F_MARGIN_CALL;
        self.emit(&e);
        self.note(J_MARGIN, now, 0, 0, e.a, e.b, 1, i32::from(e.product), 0, 0);
        self.lanes[l].account_next_ns = now;
    }

    /// A `riskLevelChange` (PM's stream, PM Pro's risk stream; BX7 C2/C3)
    /// or options' `RISK_LEVEL_CHANGE` (BX9): `NORMAL` changes nothing; a
    /// margin call is BX-20's — PM's figures the event's own (maintenance
    /// over equity), options' the lane's last — and the account read again
    /// now; a reduce-only or liquidating account — or a level this build
    /// does not know — is a `VenueLock`.
    fn on_risk_level(&mut self, l: usize, ev: &UserEvent, now: u64) {
        let unified = self.lanes[l].unified;
        match ev.risk {
            RISK_NORMAL => {}
            RISK_MARGIN_CALL if unified => {
                self.pm_margin(um_ratio_1e6(ev.qty_1e6, ev.px_1e6), now, EVT_F_MARGIN_CALL);
                self.lanes[l].account_next_ns = now;
            }
            RISK_MARGIN_CALL => self.margin_call(l, now),
            _ if self.lanes[l].product == PRODUCT_OPTIONS => self.opt_lock(now),
            _ => self.venue_lock(CODE_PM_RISK, now),
        }
    }

    /// BX9: the options account can no longer open (reduce-only, unable to
    /// trade): a `VenueLock`, raised once a process.
    fn opt_lock(&mut self, now: u64) {
        if !self.opt_locked {
            self.opt_locked = true;
            self.venue_lock(CODE_OPT_ACCOUNT, now);
        }
    }

    /// The unified account's one ratio (PM and PM Pro, BX7 C2/C3) onto
    /// every lane's product — the arm's margin book is per product — with
    /// the session equity.
    fn pm_margin(&mut self, ratio: i64, now: u64, flags: u8) {
        let mut l = 0;
        while l < self.lanes.len() {
            if self.lanes[l].unified {
                self.lanes[l].ratio_1e6 = ratio;
                let mut e = BnEvt::new(EVT_MARGIN, now);
                e.product = self.lanes[l].product;
                e.a = ratio;
                e.b = self.equity_1e6;
                e.c = self.anchor_seen();
                e.flags = flags;
                self.emit(&e);
                self.note(J_MARGIN, now, 0, 0, ratio, self.equity_1e6, i64::from(flags != 0), i32::from(e.product), 0, 0);
            }
            l += 1;
        }
    }

    /// The account is restricted, liquidating or reconfigured under us
    /// (PM's and PM Pro's risk and status, `ACCOUNT_CONFIG_UPDATE`): a
    /// `VenueLock` (the arm halts its slot). Not quiet — the venue banned
    /// nothing; the halt's sweep and the reads go on.
    fn venue_lock(&mut self, code: i32, now: u64) {
        let mut e = BnEvt::new(EVT_LOCK, now);
        e.code = code;
        self.emit(&e);
        self.note(J_VENUE, now, 0, 0, 0, 0, 0, code, 0, 0);
        if self.phase != Phase::Ready {
            self.refuse_boot(BootErr::VenueLock(code));
        }
    }

    /// Queue Portfolio Margin's unified account read on the first lane:
    /// `true` when one is coming — queued now, or already queued or in
    /// flight (its answer serves) — `false` only when the queue is full.
    fn queue_pm_account(&mut self) -> bool {
        if self.pm_account_queued {
            return true;
        }
        if !self.queue_job(0, JobKind::PmAccount, 0) {
            return false;
        }
        self.pm_account_queued = true;
        true
    }

    /// The unified account answered — `GET /papi/v1/account` (PM, BX7 C2)
    /// or `GET /sapi/v1/portfolio/account` (PM Pro, C3), the same fields:
    /// the ratio (maintenance over equity) on every product, the session
    /// equity (E7), a margin call or a lock by the account's status. A
    /// cycle waiting on it ([`ReconStep::Unified`]) judges now — or fails,
    /// on an answer that is not a 200 that scans (fail closed: no verdict
    /// on a margin and a status unread).
    fn pm_account_answered(&mut self, l: usize, status: u16, b0: usize, b1: usize, now: u64, sapi: bool) {
        let snap = if status == 200 { scan_pm_account(self.body(l, sapi, b0, b1)).ok() } else { None };
        let Some(snap) = snap else {
            if status == 200 {
                self.scan_failed(SCAN_ACCOUNT, ScanErr::Malformed, now);
            }
            if self.recon == ReconStep::Unified {
                self.recon_failed();
            }
            return;
        };
        self.equity_1e6 = snap.equity_1e6;
        self.equity_known = true;
        let flags = if snap.status == PM_MARGIN_CALL { EVT_F_MARGIN_CALL } else { 0 };
        self.pm_margin(um_ratio_1e6(snap.maint_1e6, snap.equity_1e6), now, flags);
        if snap.status == PM_LOCKED {
            self.venue_lock(CODE_PM_ACCOUNT, now);
        }
        if self.recon == ReconStep::Unified {
            self.recon = ReconStep::Idle;
            self.cycle_verdict(now);
        }
    }

    fn on_order_event(&mut self, ev: &UserEvent, row: u16, class: CidClass, asset: u8, fee_row: u16, now: u64) {
        let trade = ev.exec == X_TRADE || ev.exec == X_CALCULATED;
        if row == ROW_NONE {
            if trade {
                self.t[crate::arm::GW_FILLS_UNOWNED] += 1;
            }
            return;
        }
        let owner = self.knobs.owner_slot;
        // The order: in the table, or remembered ending (a late trade);
        // whose it is (`TRADE_BY_*`).
        let (ix, ended, slot, order_id, by) = match class {
            CidClass::Ours { slot, client_oid } => match self.oot.by_placement(slot, client_oid) {
                // S7: an event naming another venue order under this id is
                // the previous order of a reused id, not this one.
                Some(ix) if self.same_venue_order(ix, ev.venue_oid) => (Some(ix), None, slot, self.oot.get(ix).current_oid, TRADE_BY_MEMBER),
                _ => (None, self.ended.find(slot, client_oid), slot, client_oid, TRADE_BY_MEMBER),
            },
            // No member id: a venue order id or an earlier boot's id could
            // collide with a live one.
            CidClass::Orphan { .. } => (None, None, owner, 0, TRADE_BY_ORPHAN),
            CidClass::Liquidation => (None, None, owner, 0, TRADE_BY_LIQUIDATION),
            CidClass::Adl => (None, None, owner, 0, TRADE_BY_ADL),
            CidClass::Settlement => (None, None, owner, 0, TRADE_BY_SETTLEMENT),
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
        self.book_event(ev, ix, ended, row, slot, order_id, by, asset, fee_row, now);
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
            // The venue's own refusal, reported (BX8, the risk review):
            // ended, never asked about again.
            S_REJECTED => self.end_order(ix, EVT_REJECT, RETIRED_REJECTED, 0, now),
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
    /// updates (`OF_STATUS_BOOKED`). BX11: a trade's commission — the
    /// update's alone — is journaled once ([`J_FEE`]), whichever report
    /// booked it; `by` says whose order traded (`TRADE_BY_*`), `asset` the
    /// commission's asset to the row (`FEE_ASSET_*`). BX8: a commission
    /// charged in an armed spot asset (`fee_row`) is paid, once, out of
    /// that row's explained balance (the module docs' balance law).
    #[allow(clippy::too_many_arguments)]
    fn book_event(
        &mut self,
        ev: &UserEvent,
        ix: Option<u16>,
        ended: Option<usize>,
        row: u16,
        slot: u8,
        order_id: u64,
        by: u8,
        asset: u8,
        fee_row: u16,
        now: u64,
    ) {
        let (settle, ours) = (by == TRADE_BY_SETTLEMENT, by == TRADE_BY_MEMBER);
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
            let seen = self.tids.see(row, ev.trade_id, update);
            // An update books its trade only up to its `z` (a later trade's
            // lite report may be booked already: the rest waits for `z`).
            let q = if seen.again {
                0
            } else if update && known {
                ev.last_qty_1e6.min(ev.cum_qty_1e6.saturating_sub(booked)).max(0)
            } else {
                ev.last_qty_1e6
            };
            if q > 0 {
                self.t[crate::arm::GW_FILLS_UNRESOLVED] += (ours && !known) as u64;
                if ev.trade_ms != 0 {
                    // The trade's venue time to its booking here.
                    let lag_ms = self.clock.venue_ms(now).saturating_sub(ev.trade_ms);
                    self.stats.fill_venue_to_local.record(lag_ms.saturating_mul(MS));
                }
                let ts = self.fill_ts_ns(ev.trade_ms, now);
                let code = (ev.maker != 0) as i32;
                let flags = FILL_FLAG_SETTLEMENT * u8::from(settle);
                if self.book_fill(ix, ended, row, slot, ev.side, ev.last_px_1e6, q, code, flags, ts, now, order_id) {
                    booked = booked.saturating_add(q);
                }
            }
            if seen.fee_due {
                let w = self.wire.row(row);
                let (code, notional) = (fee_code(ev.maker != 0, asset, by, w), fee_notional_1e9(w, ev.last_px_1e6, ev.last_qty_1e6));
                self.note(J_FEE, now, order_id, ev.venue_oid, ev.last_px_1e6, notional, ev.commission_1e9, code, row, slot);
                if let Some(r) = self.rows.get_mut(fee_row as usize) {
                    // ×1e9 → ×1e8: the venue charges 8 decimals.
                    r.spot_1e8 = r.spot_1e8.saturating_sub(ev.commission_1e9 / 10);
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
        let flags = FILL_FLAG_SETTLEMENT * u8::from(settle);
        if self.book_fill(ix, ended, row, slot, ev.side, px, excess, J_FILL_UNSEEN, flags, ts, now, order_id) {
            match (ix, ended) {
                (Some(i), _) => self.oot.get_mut(i).flags |= OF_STATUS_BOOKED,
                (None, Some(k)) => self.ended.set_status_booked(k),
                (None, None) => {}
            }
            // BX8: no report carried the lost trades' commissions.
            if self.rows[row as usize].spot_k != u8::MAX {
                self.queue_order_trades(row, ev.venue_oid, ev.cum_qty_1e6);
            }
        }
    }

    /// A fill's timestamp ([`fill_wall_ns`]) at `now`.
    #[inline]
    fn fill_ts_ns(&self, venue_ms: u64, now: u64) -> u64 {
        fill_wall_ns(venue_ms, self.clock.venue_ms(now))
    }

    /// **Book one fill** stamped `ts_ns` (wall), flagged `flags` (a
    /// settlement; an adoption, BX7): lane 4 first, the position, the
    /// order's booked quantity and quote (in the table, or remembered
    /// ending), the journal (`code`: [`J_FILL`]'s bits), and the position
    /// event when the row's sign changes. `false`: the fill was LOST (lane 4
    /// and the held queue are both full) and nothing was booked.
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
        code: i32,
        flags: u8,
        ts_ns: u64,
        now: u64,
        order_id: u64,
    ) -> bool {
        let sym = self.wire.row(row).sym;
        let s = if side == Side::Ask as u8 { Side::Ask } else { Side::Bid };
        let f = Fill::new(ts_ns, sym, s, Price(px), Qty(qty), order_id)
            .with_attribution(slot, FILL_ORIGIN_VENUE)
            .with_flags(flags);
        let settle = flags & FILL_FLAG_SETTLEMENT != 0;
        let signed = if s == Side::Bid { qty } else { -qty };
        if !self.out_fill(&f) {
            // Nothing downstream drains lane 4. Nothing is booked — the
            // engine never saw it, so the venue's position must read as
            // drift — and the pulse raises unbounded drift (EVT_F_LOST).
            self.note(J_FILL, now, order_id, 0, px, signed, 0, code | J_FILL_LOST, row, slot);
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
        if r.spot_k != u8::MAX {
            // BX8: a spot fill moves its base asset's explained balance
            // (×1e8; the quantity is on the step, so exact).
            let d = qty.saturating_mul(100);
            r.spot_1e8 = if s == Side::Bid { r.spot_1e8.saturating_add(d) } else { r.spot_1e8.saturating_sub(d) };
        }
        if px > 0 {
            r.last_px_1e6 = px;
        }
        let after = r.booked_1e6;
        self.note(J_FILL, now, order_id, 0, px, signed, 0, code, row, slot);
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
    fn queue_job(&mut self, l: usize, kind: JobKind, row: u16) -> bool {
        if self.lanes[l].jobs.push(Job { kind, row, lane: l as u8, sent_ns: 0, oid: 0, tries: 0, qty_1e6: 0, not_before_ns: 0 }) {
            return true;
        }
        self.t[crate::arm::GW_JOBS_DROPPED] += 1;
        false
    }

    /// **Ask for a spot order's trades** (BX8): a fill of venue order
    /// `venue_oid` on spot row `row` was booked without its report — its
    /// commission is owed to the explained balance ([`JobKind::OrderTrades`]);
    /// the order has executed `executed_1e6` in all. A full queue is
    /// counted; the commission then reads as drift (fail closed).
    fn queue_order_trades(&mut self, row: u16, venue_oid: u64, executed_1e6: i64) {
        let l = self.lane_of_row(row);
        let j = Job { kind: JobKind::OrderTrades, row, lane: l as u8, sent_ns: 0, oid: venue_oid, tries: 0, qty_1e6: executed_1e6, not_before_ns: 0 };
        if venue_oid == 0 || !self.lanes[l].jobs.push(j) {
            self.t[crate::arm::GW_JOBS_DROPPED] += 1;
        }
    }

    /// A spot order's trades went unread, or read short: asked again
    /// behind the queue after [`ORDER_TRADES_BACKOFF_NS`] × the tries, up to
    /// [`ORDER_TRADES_TRIES`] (the risk review) — then counted, the
    /// commission left unpaid (drift, fail closed).
    fn retry_order_trades(&mut self, j: Job, now: u64) {
        let tries = j.tries.saturating_add(1);
        let again = Job {
            tries,
            sent_ns: 0,
            not_before_ns: now.saturating_add(ORDER_TRADES_BACKOFF_NS * u64::from(tries)),
            ..j
        };
        if again.tries >= ORDER_TRADES_TRIES || !self.lanes[j.lane as usize].jobs.push(again) {
            self.t[crate::arm::GW_JOBS_DROPPED] += 1;
            self.order_trades_done(now);
        }
    }

    /// A spot order's trades read left the queue for good (paid, or given
    /// up): its row's leg is held no more ([`Self::hold_owed_legs`]), so the
    /// next cycle is asked for now — a held cycle's pause lasts one round
    /// trip, not a cadence (the risk review's R2-1).
    #[inline]
    fn order_trades_done(&mut self, now: u64) {
        self.recon_next_ns = now;
    }

    /// A job left the queue for good (answered, failed or dropped): its
    /// row may queue another of its kind.
    fn job_done(&mut self, j: &Job) {
        self.probe_queued &= j.kind != JobKind::Probe;
        self.pm_account_queued &= j.kind != JobKind::PmAccount;
        self.exercise_queued &= j.kind != JobKind::Exercise;
        if let Some(r) = self.rows.get_mut(j.row as usize) {
            match j.kind {
                JobKind::Countdown => r.cd_queued = false,
                JobKind::UserTrades => r.day_queued = false,
                JobKind::CancelAll => r.cancel_all_queued = false,
                _ => {}
            }
        }
    }

    /// The job goes to the SAPI connection: the key's and the account
    /// mode's assertions — and in Portfolio Margin the clock (papi has no
    /// time endpoint).
    fn is_sapi(&self, k: JobKind) -> bool {
        matches!(k, JobKind::Restrictions | JobKind::RestrictionsOpt | JobKind::Portfolio)
            || (self.pm && k == JobKind::Time)
            || (self.pm_pro && k == JobKind::PmAccount)
    }

    /// Start the next REST job if its connection is free (between poll
    /// batches, BX5) — nothing while quiet.
    fn start_jobs(&mut self, l: usize, now: u64) {
        if now < self.quiet_until_ns {
            return;
        }
        if self.sapi_job.is_none() {
            if let Some(j) = self.lanes[l].jobs.front() {
                if self.is_sapi(j.kind) {
                    self.lanes[l].jobs.pop();
                    self.start_rest(l, j, now, true);
                }
            }
        }
        if self.lanes[l].job.is_none() {
            if let Some(j) = self.lanes[l].jobs.front() {
                if !self.is_sapi(j.kind) {
                    self.lanes[l].jobs.pop();
                    // BX9 (Q-5): an options day read waits while eapi's
                    // window could not take it beside a whole cycle.
                    let paced = j.kind == JobKind::UserTrades && self.lanes[l].product == PRODUCT_OPTIONS && !self.opt_day_room(now);
                    if now < j.not_before_ns || paced {
                        // A spot order's trades asked again after a pause
                        // (BX8), an options day read paced (BX9): behind the
                        // queue until its time — the jobs behind it go first
                        // (just popped: there is room).
                        let _ = self.lanes[l].jobs.push(j);
                    } else {
                        self.start_rest(l, j, now, false);
                    }
                }
            }
        }
    }

    fn start_rest(&mut self, l: usize, mut j: Job, now: u64, sapi: bool) {
        if j.kind == JobKind::Countdown && !self.ready_to_send(l, now) {
            // N2: a renewal queued while the session could cancel is not
            // sent once it cannot (B1).
            return self.job_done(&j);
        }
        let ts = self.clock.venue_ms(now);
        let rw = self.knobs.recv_window_ms;
        let (t, signed) = match j.kind {
            JobKind::Time => (if sapi { S_TIME } else { T_TIME }, false),
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
            // PM's unified account is papi's; PM Pro's, SAPI's.
            JobKind::PmAccount => (if sapi { S_PORTFOLIO } else { T_ACCOUNT }, true),
            JobKind::SymbolConfig => (T_SYMBOL_CONFIG, true),
            JobKind::Price => (T_PRICE, false),
            JobKind::OrderTrades => (T_USER_TRADES, true),
            JobKind::OptPositions => (T_POSITION, true),
            JobKind::Exercise => (T_EXERCISE, true),
            JobKind::RestrictionsOpt => (S_API_RESTRICTIONS_OPT, true),
        };
        // BX9 (Q-1): the options lane's jobs, and the options key's own
        // permission read, sign with the options signer.
        let opt = self.lanes[l].product == PRODUCT_OPTIONS && !sapi;
        let opt_signed = opt || j.kind == JobKind::RestrictionsOpt;
        let per_row = matches!(
            j.kind,
            JobKind::Countdown
                | JobKind::UserTrades
                | JobKind::CancelAll
                | JobKind::Probe
                | JobKind::SymbolConfig
                | JobKind::Price
                | JobKind::OrderTrades
        );
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
                // boot has nowhere to go (a cycle's unified read cannot be
                // one — PM and PM Pro keep it — but would fail the cycle).
                None => {
                    if j.kind == JobKind::PmAccount && self.recon == ReconStep::Unified {
                        self.recon_failed();
                    }
                    return self.job_done(&j);
                }
            }
        } else {
            &mut self.lanes[l].rest
        };
        let win = conn.window_mut(t);
        if win.is_empty() && signed {
            // A dial is pending: the window comes back next pass (the job
            // was just popped, so there is room for it).
            let _ = self.lanes[l].jobs.push(j);
            return;
        }
        let mut w = QueryWriter::new(win);
        if per_row {
            if opt && j.kind == JobKind::Price {
                // BX9: an option underlying's index (`/eapi/v1/index`).
                w.put(b"underlying=").put(self.wire.underlying(self.wire.opt(j.row).uly));
            } else {
                w.put(b"symbol=").put(self.wire.row(j.row).symbol());
            }
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
                JobKind::OrderTrades => {
                    w.put(b"&orderId=").uint(j.oid);
                }
                _ => {}
            }
            // The signed suffix follows; an unsigned query ends here.
            if signed {
                w.put(b"&");
            }
        }
        if j.kind == JobKind::Exercise {
            // BX9: every settlement since the earliest settling expiry.
            w.put(b"startTime=").uint(self.exercise_from_ms).put(b"&limit=").uint(EXERCISE_PAGE as u64).put(b"&");
        }
        let len = match (signed, opt_signed, self.opt_signer.as_ref()) {
            (false, _, _) => w.rendered(),
            (true, false, _) => {
                w.put(b"recvWindow=").uint(rw).put(b"&timestamp=").uint(ts);
                w.sign_with(&self.signer)
            }
            (true, true, Some(sg)) => {
                w.put(b"recvWindow=").uint(rw).put(b"&timestamp=").uint(ts);
                w.sign_with(sg)
            }
            // The boot refuses options with no signer: unreachable.
            (true, true, None) => return self.rest_failed(j, now),
        };
        let Ok(len) = len else {
            return self.rest_failed(j, now);
        };
        j.sent_ns = now;
        match conn.start(t, len, self.poll.registry(), now) {
            Ok(()) => {
                self.weigh(l, job_weight(j.kind, self.lanes[l].product), now);
                if sapi {
                    self.sapi_job = Some(j);
                } else {
                    self.lanes[l].job = Some(j);
                }
            }
            Err(_) => {
                // Busy or the dial failed: offer it again next pass (just
                // popped: there is room).
                let _ = self.lanes[l].jobs.push(j);
            }
        }
    }

    /// The answer body of the last REST request on its connection.
    #[inline]
    fn body(&self, l: usize, sapi: bool, b0: usize, b1: usize) -> &[u8] {
        let conn = match (sapi, self.sapi.as_ref()) {
            (true, Some(c)) => c,
            _ => &self.lanes[l].rest,
        };
        conn.resp().get(b0..b1).unwrap_or(&[])
    }

    fn on_rest_progress(&mut self, l: usize, p: Progress, now: u64, sapi: bool) {
        let (status, span) = match p {
            Progress::Waiting => return,
            Progress::Done { status, body } => (status, body),
            Progress::Failed(_) => {
                let j = if sapi { self.sapi_job.take() } else { self.lanes[l].job.take() };
                if let Some(j) = j {
                    self.rest_failed(j, now);
                }
                return;
            }
        };
        let j = if sapi { self.sapi_job.take() } else { self.lanes[l].job.take() };
        let Some(j) = j else { return };
        self.stats.rest_rtt.record(now.saturating_sub(j.sent_ns));
        if status != 200 {
            let (code, until) = {
                let b = self.body(l, sapi, span.start, span.end);
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
        let l = j.lane as usize;
        self.job_done(&j);
        match j.kind {
            JobKind::OpenOrders => self.recon_failed(),
            // BX9: the cycle waits on the positions; the exercise records
            // are asked again by the next cycle while a leg is held.
            JobKind::OptPositions => {
                if self.recon == ReconStep::Positions {
                    self.recon_failed();
                }
            }
            JobKind::Exercise => {}
            JobKind::Time => {
                self.clock_pending = self.clock_pending.saturating_sub(1);
                if self.clock_pending == 0 && !self.clock.measured() {
                    self.start_clock_round(now);
                } else if self.clock_pending > 0 {
                    let _ = self.queue_job(0, JobKind::Time, 0);
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
            // next probe goes at its time. A spot price unread leaves its
            // leg unpriced: the boot's adoption refuses it (BX8).
            JobKind::CancelAll | JobKind::Probe | JobKind::Price => {}
            // A spot order's trades: asked again, bounded (BX8).
            JobKind::OrderTrades => self.retry_order_trades(j, now),
            // A cycle waiting on the unified account fails (BX7); a
            // re-read's next one asks again.
            JobKind::PmAccount => {
                if self.recon == ReconStep::Unified {
                    self.recon_failed();
                }
            }
            JobKind::Dual
            | JobKind::MultiAssets
            | JobKind::Restrictions
            | JobKind::RestrictionsOpt
            | JobKind::Portfolio
            | JobKind::SymbolConfig
            | JobKind::ListenNew
            | JobKind::ListenKeep => {
                let _ = self.queue_job(l, j.kind, j.row);
            }
        }
    }

    fn rest_answered(&mut self, j: Job, status: u16, b0: usize, b1: usize, now: u64, sapi: bool) {
        let l = j.lane as usize;
        self.job_done(&j);
        match j.kind {
            JobKind::Time => {
                let t = scan_server_time(self.body(l, sapi, b0, b1));
                if let (200, Ok(ms)) = (status, t) {
                    self.clock.sample(j.sent_ns, now, ms);
                }
                self.clock_pending = self.clock_pending.saturating_sub(1);
                if self.clock_pending > 0 {
                    let _ = self.queue_job(0, JobKind::Time, 0);
                } else if self.phase == Phase::Clock {
                    if self.clock.measured() {
                        self.phase = Phase::Assert;
                        self.queue_asserts();
                    } else {
                        self.start_clock_round(now);
                    }
                }
            }
            JobKind::Dual
            | JobKind::MultiAssets
            | JobKind::Restrictions
            | JobKind::RestrictionsOpt
            | JobKind::Portfolio
            | JobKind::SymbolConfig => {
                let b = self.body(l, sapi, b0, b1);
                let r = match j.kind {
                    JobKind::Dual if status == 200 => judge_one_way(b),
                    JobKind::MultiAssets if status == 200 => judge_single_asset(b),
                    JobKind::SymbolConfig if status == 200 => {
                        judge_symbol_config(b, self.wire.row(j.row).symbol(), self.knobs.max_leverage)
                    }
                    // The arm's key: what it signs — options too when it is
                    // the options key (BX9, Q-1); the options key apart.
                    JobKind::Restrictions if status == 200 => judge_key(
                        b,
                        self.knobs.mode,
                        self.has_futures(),
                        self.lane_ix[PRODUCT_SPOT as usize] != NO_LANE,
                        self.lane_ix[PRODUCT_OPTIONS as usize] != NO_LANE && !self.opt_own_key,
                    ),
                    JobKind::RestrictionsOpt if status == 200 => judge_key(b, self.knobs.mode, false, false, true),
                    JobKind::Portfolio => judge_mode(status, b, self.knobs.mode),
                    _ => Err(AssertErr::Unreadable),
                };
                if let Err(e) = r {
                    self.refuse_boot(BootErr::Assert(e));
                    return;
                }
                self.asserts_left = self.asserts_left.saturating_sub(1);
                if self.asserts_left == 0 && self.phase == Phase::Assert {
                    self.asserts_held();
                }
            }
            JobKind::ListenNew => self.listen_key_answered(l, status, b0, b1, now),
            JobKind::ListenKeep => {
                if status == 200 {
                    self.lanes[l].user.kept_ns = now;
                } else {
                    // The key is gone: a new one, a new socket.
                    self.lanes[l].user.expire();
                    let _ = self.queue_job(l, JobKind::ListenNew, 0);
                }
            }
            JobKind::Countdown => {
                let ok = status == 200 && scan_countdown(self.body(l, sapi, b0, b1)).is_ok();
                if let Some(r) = self.rows.get_mut(j.row as usize) {
                    if ok {
                        r.cd_ok_ns = now;
                    }
                    r.cd_bad = !ok;
                }
                self.cd_proven |= ok;
            }
            JobKind::Probe => {
                if status == 200 && scan_countdown(self.body(l, sapi, b0, b1)).is_ok() {
                    self.cd_proven = true;
                } else {
                    // N1: a row refused for good (`-4411`) must not block
                    // every maker — the next probe asks the next row.
                    self.cd_probe_row = self.next_probe_row(self.cd_probe_row);
                }
            }
            JobKind::OpenOrders => self.orders_answered(l, status, b0, b1, j.sent_ns, now),
            // BX8: the price a spot balance the boot adopts is valued at — its
            // leg's, for this cycle's adoption (an unread price leaves the
            // leg unpriced: the adoption refuses it, fail closed).
            // BX9: an option underlying's index, for a held short's adoption
            // (unread, the adoption refuses it, fail closed).
            JobKind::Price if self.lanes[l].product == PRODUCT_OPTIONS => {
                if status == 200 && (j.row as usize) < self.wire.len() {
                    let uly = self.wire.opt(j.row).uly as usize;
                    if let (Ok(px), true) = (scan_opt_index(self.body(l, sapi, b0, b1)), uly < OPT_ULY_MAX) {
                        self.opt_index_1e6[uly] = px;
                    }
                }
            }
            JobKind::Price => {
                if status == 200 {
                    let px = if (j.row as usize) < self.wire.len() {
                        scan_ticker_price(self.body(l, sapi, b0, b1), self.wire.row(j.row).symbol())
                    } else {
                        Err(ScanErr::Missing)
                    };
                    if let (Ok(px), Some(g)) = (px, self.legs.get_mut(j.row as usize)) {
                        g.px_1e6 = px;
                    }
                }
            }
            JobKind::OptPositions => self.opt_positions_answered(l, status, b0, b1, now),
            JobKind::Exercise => self.exercise_answered(l, status, b0, b1, now),
            JobKind::OrderTrades if status != 200 => self.retry_order_trades(j, now),
            JobKind::OrderTrades => {
                if !self.order_trades_answered(l, j, b0, b1, now) {
                    self.retry_order_trades(j, now);
                }
            }
            JobKind::PmAccount => self.pm_account_answered(l, status, b0, b1, now, sapi),
            JobKind::UserTrades => self.trades_answered(l, j.row, status, b0, b1, now),
            // The orders' ends arrive on the user stream, or the next
            // listing puts them in doubt.
            JobKind::CancelAll => {}
        }
    }

    /// A listenKey was created: the stream (re)connects on it. A key that
    /// does not fit or is not `[A-Za-z0-9]` is refused whole and asked for
    /// again, never truncated.
    fn listen_key_answered(&mut self, l: usize, status: u16, b0: usize, b1: usize, now: u64) {
        let adopted = {
            let b = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
            match (status, scan_listen_key(b)) {
                // Adopted straight from the answer (the stream keeps its
                // own copy: the next request overwrites the answer).
                (200, Ok(ks)) => self.lanes[l].user.set_key(ks.get(b), now),
                _ => None,
            }
        };
        let Some(changed) = adopted else {
            let _ = self.queue_job(l, JobKind::ListenNew, 0);
            return;
        };
        if changed || self.lanes[l].us.is_down() {
            // The path the socket upgrades to: `/ws/` (PM: `/pm/ws/`) and the
            // key, laid straight into the socket's own path (cold, once per
            // key) — and PM Pro's risk stream, on the same key.
            let ln = &mut self.lanes[l];
            let (pre, key) = (ln.us_prefix, ln.user.key());
            if !ln.us.is_down() {
                ln.us.close();
            }
            let _ = ln.us.set_path_parts(&[pre, key]);
            if let Some(r) = ln.risk.as_mut() {
                if !r.ws.is_down() {
                    r.ws.close();
                }
                let _ = r.ws.set_path_parts(&[&b"/pm-classic/ws/"[..], key]);
                r.retry_ns = 0;
            }
            ln.us_retry_ns = 0;
        }
        if self.phase == Phase::ListenKey && self.all_keyed() {
            self.phase = Phase::Connect;
        }
    }

    // ---------------------------------------------------------------------
    // Reconciliation, margin, the day
    // ---------------------------------------------------------------------

    /// Start a reconciliation from lane `l` — each lane's account read
    /// then its open orders, in lane order, and one verdict over every leg
    /// (BX7, M3) — or, `account_only`, lane `l`'s margin re-read (S5).
    /// `false`: not started (one in flight, the lane cannot send, no
    /// request id).
    fn start_recon(&mut self, l: usize, now: u64, account_only: bool) -> bool {
        if self.recon != ReconStep::Idle || !self.recon_account(l, now, account_only) {
            return false;
        }
        if !account_only {
            // The cycle's accumulators start empty.
            self.verdict = Verdict::default();
            self.recon_ours = 0;
        }
        true
    }

    /// Lane `l`'s account read: a cycle's step, or a margin re-read.
    fn recon_account(&mut self, l: usize, now: u64, account_only: bool) -> bool {
        if !self.ready_to_send(l, now) || self.post(l, WsReq::on(REQ_ACCOUNT, IX_NONE, 0, 0), now).is_err() {
            return false;
        }
        self.recon = ReconStep::Account;
        self.recon_lane = l as u8;
        self.recon_account_only = account_only;
        let ln = &mut self.lanes[l];
        ln.account_read_ns = now;
        // A full cycle's account read answers a re-read asked for (N-e).
        if !account_only {
            ln.account_next_ns = 0;
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

    fn account_answered(&mut self, fr: Frame, a: &WsAnswer, now: u64) {
        if a.status != 200 {
            return self.recon_failed();
        }
        let ln = fr.lane();
        let product = self.lanes[ln].product;
        if product == PRODUCT_SPOT {
            return self.spot_account_answered(fr, a, now);
        }
        if product == PRODUCT_OPTIONS {
            return self.opt_account_answered(fr, a, now);
        }
        // The result lies inside the frame its session still holds (or the
        // REST route's body). USDⓈ-M: the ratio and the session equity
        // (E7); COIN-M: the riskiest asset's ratio — no USD equity.
        let scanned = {
            let frame = frame_of(&self.lanes, fr);
            if self.lanes[ln].pm {
                // Portfolio Margin: the product's positions alone; the
                // margin is the unified account's (`T_ACCOUNT`).
                scan_positions(frame, a.result, &mut self.pos[..]).map(|n| (None, None, n))
            } else if product == PRODUCT_COINM {
                scan_cm_account(frame, a.result, &mut self.pos[..]).map(|s| (Some(s.ratio_1e6), None, s.n_pos))
            } else {
                scan_um_account(frame, a.result, &mut self.pos[..])
                    .map(|s| (Some(um_ratio_1e6(s.maint_1e6, s.margin_balance_1e6)), Some(s.margin_balance_1e6), s.n_pos))
            }
        };
        let Ok((ratio, equity, n_pos)) = scanned else {
            self.scan_failed(SCAN_ACCOUNT, ScanErr::Malformed, now);
            return self.recon_failed();
        };
        // PM Pro reads its classic account for the positions; its margin is
        // the unified account's (as Portfolio Margin's).
        let (ratio, equity) = if self.lanes[ln].unified { (None, None) } else { (ratio, equity) };
        let account_only = self.recon_account_only;
        // BX-19 (BX7): a classic COIN-M read lists every symbol with its
        // margin type and leverage — judged on every read, in the pass below.
        let cm = product == PRODUCT_COINM && !self.lanes[ln].pm;
        // The lane's positions onto its legs (a full reconciliation's). A
        // contract of difference on an inverse row is worth its face,
        // whatever the price (the inverse law): that is its leg's price.
        let mut i = 0;
        while i < self.legs.len() && !account_only {
            let w = self.wire.row(i as u16);
            if self.lane_ix[w.product as usize] as usize == ln {
                let face = (w.flags & INST_INVERSE != 0).then_some(w.unit_1e6);
                self.legs[i].venue_1e6 = 0;
                self.legs[i].booked_1e6 = self.rows[i].booked_1e6;
                self.legs[i].px_1e6 = face.unwrap_or(self.rows[i].last_px_1e6);
            }
            i += 1;
        }
        // One pass: each bound row the read lists, marked with the read's
        // generation — listed twice fails the read (BX7: the later entry
        // could mask the earlier, an unbooked position read as flat, an
        // unlisted row as listed).
        self.read_gen = self.read_gen.wrapping_add(1).max(1);
        let gen = self.read_gen;
        let cap = u16::from(self.knobs.max_leverage);
        let (mut isolated, mut over) = (false, false);
        let mut k = 0;
        while k < n_pos {
            let p = &self.pos[k];
            k += 1;
            let row = self.wire.find(product, p.symbol.get(self.frame(fr)));
            if row == ROW_NONE {
                if !account_only && p.amt_1e6 != 0 && !self.knobs.shared {
                    // A dedicated account holding what the boot did not bind.
                    self.verdict.unseen_legs += 1;
                }
                continue;
            }
            let r = &mut self.rows[row as usize];
            if r.listed == gen {
                self.scan_failed(SCAN_ACCOUNT, ScanErr::Malformed, now);
                return self.recon_failed();
            }
            r.listed = gen;
            let w = self.wire.row(row);
            if cm && w.flags & INST_OWNED != 0 {
                isolated |= p.margin == MARGIN_ISOLATED;
                over |= p.leverage > cap;
            }
            if !account_only {
                let l = &mut self.legs[row as usize];
                l.venue_1e6 = p.amt_1e6;
                if p.amt_1e6 != 0 && w.flags & INST_INVERSE == 0 {
                    l.px_1e6 = ((p.notional_1e6.saturating_abs() as i128 * 1_000_000) / p.amt_1e6.saturating_abs() as i128) as i64;
                }
            }
        }
        if cm {
            self.judge_cm_margin(ln, gen, isolated, over, now);
        }
        // Margin (BX-20), per product; USDⓈ-M's margin balance is the
        // session equity's (E7). Portfolio Margin's comes from the unified
        // account instead.
        if let Some(ratio) = ratio {
            self.lanes[ln].ratio_1e6 = ratio;
            if let Some(eq) = equity {
                self.um_equity_1e6 = eq;
                self.um_read = true;
                self.refresh_equity();
            }
            self.margin_sample(product, ratio, now);
        }
        if account_only {
            self.recon = ReconStep::Idle;
            self.recon_account_only = false;
            return;
        }
        self.recon = ReconStep::Orders;
        if !self.queue_job(ln, JobKind::OpenOrders, 0) {
            self.recon_failed();
        }
    }

    /// **A spot lane's account read** (BX8): the armed base assets'
    /// balances onto their legs — ×1e8 read, compared at ×1e6 against the
    /// balances the bookings explain (the module docs' balance law); the
    /// account's `canTrade` and permissions judged for every live armed
    /// row (BX-19, the bStock check: before Ready a refusal refuses the
    /// boot, after it is a `VenueLock`, raised once); and, the boot's first
    /// cycle, each balance the boot will adopt priced (`ticker/price`, on
    /// the lane's REST queue ahead of its listing, so it lands first). No
    /// margin, no equity.
    fn spot_account_answered(&mut self, fr: Frame, a: &WsAnswer, now: u64) {
        let ln = fr.lane();
        let scanned = {
            let frame = frame_of(&self.lanes, fr);
            scan_spot_account(frame, a.result, &self.spot_assets[..self.n_spot], &mut self.spot_bal_1e8[..], &mut self.acct_perm)
        };
        let Ok(snap) = scanned else {
            self.scan_failed(SCAN_ACCOUNT, ScanErr::Malformed, now);
            return self.recon_failed();
        };
        let mut refused = if snap.can_trade { None } else { Some(AssertErr::CannotTrade) };
        let mut k = 0;
        while k < self.n_spot && refused.is_none() {
            let row = self.spot_rows[k];
            if self.wire.row(row).flags & INST_LIVE != 0 {
                refused = judge_permission(&self.wire.spot(row).perm, &self.acct_perm).err();
            }
            k += 1;
        }
        if let Some(e) = refused {
            if self.phase != Phase::Ready {
                self.refuse_boot(BootErr::Assert(e));
                return self.recon_failed();
            }
            if !self.perm_locked {
                self.perm_locked = true;
                self.venue_lock(CODE_PERMISSION, now);
            }
        }
        if self.recon_account_only {
            self.recon = ReconStep::Idle;
            self.recon_account_only = false;
            return;
        }
        let first = self.phase == Phase::Recon && !self.adopt_ran;
        let mut k = 0;
        while k < self.n_spot {
            let i = self.spot_rows[k] as usize;
            let g = &mut self.legs[i];
            g.venue_1e6 = self.spot_bal_1e8[k].div_euclid(100);
            g.booked_1e6 = self.rows[i].spot_1e8.div_euclid(100);
            g.px_1e6 = self.rows[i].last_px_1e6;
            g.held = false;
            k += 1;
        }
        self.hold_owed_legs(ln);
        let mut k = 0;
        while first && k < self.n_spot {
            let row = self.spot_rows[k];
            if self.adoptable(row as usize) && !self.queue_job(ln, JobKind::Price, row) {
                return self.recon_failed();
            }
            k += 1;
        }
        self.recon = ReconStep::Orders;
        if !self.queue_job(ln, JobKind::OpenOrders, 0) {
            self.recon_failed();
        }
    }

    /// A product's margin sample (BX-20): its ratio, with the session equity
    /// and the anchor — the arm's margin book, and the journal.
    fn margin_sample(&mut self, product: u8, ratio: i64, now: u64) {
        let mut e = BnEvt::new(EVT_MARGIN, now);
        e.product = product;
        e.a = ratio;
        e.b = self.equity_1e6;
        e.c = self.anchor_seen();
        self.emit(&e);
        self.note(J_MARGIN, now, 0, 0, ratio, self.equity_1e6, 0, i32::from(product), 0, 0);
    }

    /// **E7's session equity on a classic account** (BX9, Q-4): USDⓈ-M's
    /// margin balance plus the options account's equity — 0 (unread) until
    /// each armed one has been read, never a part of it passed as the whole.
    /// COIN-M has no USD equity; Portfolio Margin's and PM Pro's are the
    /// unified account's (set there, never here).
    fn refresh_equity(&mut self) {
        let um = self.lane_ix[PRODUCT_USDM as usize] != NO_LANE;
        let opt = self.lane_ix[PRODUCT_OPTIONS as usize] != NO_LANE;
        self.equity_known = (self.um_read || !um) && (self.opt_read || !opt);
        self.equity_1e6 = if self.equity_known { self.um_equity_1e6.saturating_add(self.opt_equity_1e6) } else { 0 };
    }

    /// The anchor as the arm reads it (E7): 0 — no session result — until
    /// the session equity is read whole ([`Self::equity_known`]).
    const fn anchor_seen(&self) -> i64 {
        if self.equity_known {
            self.anchor_1e6
        } else {
            0
        }
    }

    /// **The options account** (BX9; `GET /eapi/v1/marginAccount` on the
    /// route): the ratio `maintMargin / adjustedEquity` into the options
    /// product's margin, the equity into the session's (Q-4). An account
    /// that cannot trade or is reduce-only opens nothing: before Ready the
    /// boot is refused, after it a `VenueLock` (once). A full cycle's next
    /// step is the positions.
    fn opt_account_answered(&mut self, fr: Frame, a: &WsAnswer, now: u64) {
        let ln = fr.lane();
        let scanned = scan_opt_margin(frame_of(&self.lanes, fr), a.result);
        let Ok(snap) = scanned else {
            self.scan_failed(SCAN_ACCOUNT, ScanErr::Malformed, now);
            return self.recon_failed();
        };
        let refused = if !snap.can_trade {
            Some(AssertErr::CannotTrade)
        } else if snap.reduce_only {
            Some(AssertErr::ReduceOnly)
        } else {
            None
        };
        if let Some(e) = refused {
            if self.phase != Phase::Ready {
                self.refuse_boot(BootErr::Assert(e));
                return self.recon_failed();
            }
            self.opt_lock(now);
        }
        let ratio = um_ratio_1e6(snap.maint_1e6, snap.adj_equity_1e6);
        self.lanes[ln].ratio_1e6 = ratio;
        self.opt_equity_1e6 = snap.equity_1e6;
        self.opt_read = true;
        self.refresh_equity();
        self.margin_sample(PRODUCT_OPTIONS, ratio, now);
        if self.recon_account_only {
            self.recon = ReconStep::Idle;
            self.recon_account_only = false;
            return;
        }
        self.recon = ReconStep::Positions;
        if !self.queue_job(ln, JobKind::OptPositions, 0) {
            self.recon_failed();
        }
    }

    /// **The options positions** (BX9; `GET /eapi/v1/position`): the lane's
    /// legs — each listed row's signed amount, valued at its mark per
    /// contract; a row listed twice fails the read. A leg past its row's
    /// expiry and not flat is SETTLING (the module docs: not judged, the
    /// cycle conclusive) until its settlement — for at most
    /// [`EXPIRY_HOLD_MAX_MS`] — and the exercise records are asked for. At boot: a held short on an owned row the arm may not
    /// write refuses the boot ([`BootErr::OptionShort`]); each underlying a
    /// short the boot will adopt names has its index asked for (ahead of
    /// the listing, so it lands first). Then the open orders.
    fn opt_positions_answered(&mut self, l: usize, status: u16, b0: usize, b1: usize, now: u64) {
        if self.recon != ReconStep::Positions {
            return;
        }
        if status != 200 {
            return self.recon_failed();
        }
        let n = {
            let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
            scan_opt_positions(body, &mut self.pos[..])
        };
        let Ok(n) = n else {
            self.scan_failed(SCAN_REST, ScanErr::Malformed, now);
            return self.recon_failed();
        };
        let mut i = 0;
        while i < self.legs.len() {
            if self.wire.row(i as u16).product == PRODUCT_OPTIONS {
                let g = &mut self.legs[i];
                g.venue_1e6 = 0;
                g.booked_1e6 = self.rows[i].booked_1e6;
                g.px_1e6 = self.rows[i].last_px_1e6;
                g.settling = false;
            }
            i += 1;
        }
        self.read_gen = self.read_gen.wrapping_add(1).max(1);
        let gen = self.read_gen;
        let mut k = 0;
        while k < n {
            let p = &self.pos[k];
            k += 1;
            let row = {
                let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
                self.wire.find(PRODUCT_OPTIONS, p.symbol.get(body))
            };
            if row == ROW_NONE {
                if p.amt_1e6 != 0 && !self.knobs.shared {
                    // A dedicated account holding what the boot did not bind.
                    self.verdict.unseen_legs += 1;
                }
                continue;
            }
            if self.rows[row as usize].listed == gen {
                self.scan_failed(SCAN_REST, ScanErr::Malformed, now);
                return self.recon_failed();
            }
            self.rows[row as usize].listed = gen;
            let g = &mut self.legs[row as usize];
            g.venue_1e6 = p.amt_1e6;
            // A mark of 0 (a worthless option) keeps the leg's last price:
            // a drift is never valued at nothing (the parser review).
            if p.amt_1e6 != 0 && p.notional_1e6 != 0 {
                g.px_1e6 = ((p.notional_1e6.saturating_abs() as i128 * 1_000_000) / p.amt_1e6.saturating_abs() as i128) as i64;
            }
        }
        let boot = self.phase != Phase::Ready;
        let first = self.phase == Phase::Recon && !self.adopt_ran;
        let venue_ms = self.clock.venue_ms(now);
        // Underlyings whose index this read asks for (a bit each).
        let mut asked = 0u32;
        let mut hold_from = u64::MAX;
        let mut i = 0;
        while i < self.legs.len() {
            let row = i as u16;
            let w = self.wire.row(row);
            if w.product != PRODUCT_OPTIONS {
                i += 1;
                continue;
            }
            let flags = w.flags;
            let (expiry, uly) = {
                let o = self.wire.opt(row);
                (o.expiry_ms.max(0) as u64, u32::from(o.uly))
            };
            let (venue, owned) = (self.legs[i].venue_1e6, self.legs[i].owned);
            // Settling: a BOOKED leg past its expiry (R9) — the venue
            // holding what this gateway never booked is judged at once (at
            // the boot, adopted first).
            if self.legs[i].booked_1e6 != 0 && expiry != 0 && venue_ms >= expiry && venue_ms < expiry.saturating_add(EXPIRY_HOLD_MAX_MS) {
                self.legs[i].settling = true;
                hold_from = hold_from.min(expiry);
            }
            if boot && owned && venue < 0 && flags & INST_WRITABLE == 0 {
                self.refuse_boot(BootErr::OptionShort(row));
                return self.recon_failed();
            }
            if first && venue < 0 && self.adoptable(i) && uly < OPT_ULY_MAX as u32 && asked & (1 << uly) == 0 {
                asked |= 1 << uly;
                if !self.queue_job(l, JobKind::Price, row) {
                    return self.recon_failed();
                }
            }
            i += 1;
        }
        if hold_from != u64::MAX && !self.exercise_queued && self.queue_job(l, JobKind::Exercise, 0) {
            self.exercise_queued = true;
            self.exercise_from_ms = hold_from;
        }
        self.recon = ReconStep::Orders;
        if !self.queue_job(l, JobKind::OpenOrders, 0) {
            self.recon_failed();
        }
    }

    /// **The exercise records** (BX9; `GET /eapi/v1/exerciseRecord`): each
    /// names an expired option's settlement. A settling row still booked is
    /// flattened by ONE settlement fill (`FILL_FLAG_SETTLEMENT`: the ledger
    /// zeroes the row, no turnover) at its intrinsic value per contract —
    /// `max(±(S − K), 0) × unit` at the record's settlement price `S` —
    /// the closing side of the booked position; the record's fee journaled
    /// as the settlement's commission. A record of a row not booked (or
    /// booked already) books nothing: the venue's position, read next
    /// cycle, judges it. A record that disagrees with the booked leg
    /// ([`settlement_agrees`]: not the whole position, the cash's sign not
    /// the side's, or its cash per contract not the intrinsic value; the
    /// risk review's R4) — or a page naming the row twice — books nothing
    /// and is a scan failure (the slot's drift halt); the leg stays
    /// settling until
    /// [`EXPIRY_HOLD_MAX_MS`], then is judged. Unread or unscannable: the
    /// rows stay settling, the next cycle asks again.
    fn exercise_answered(&mut self, l: usize, status: u16, b0: usize, b1: usize, now: u64) {
        if status != 200 {
            return;
        }
        let n = {
            let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
            scan_exercise(body, &mut self.exercise[..])
        };
        let Ok(n) = n else {
            self.scan_failed(SCAN_REST, ScanErr::Malformed, now);
            return;
        };
        let slot = self.knobs.owner_slot;
        let venue_ms = self.clock.venue_ms(now);
        let mut k = 0;
        while k < n {
            // COPY: one record (48 B) off the read's array — booking it
            // borrows the whole gateway — rejected: re-reading each field
            // through the array around the booking.
            let x = self.exercise[k];
            k += 1;
            let row = {
                let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
                self.wire.find(PRODUCT_OPTIONS, x.symbol.get(body))
            };
            if row == ROW_NONE {
                continue;
            }
            let i = row as usize;
            let booked = self.rows[i].booked_1e6;
            let expiry = self.wire.opt(row).expiry_ms.max(0) as u64;
            if booked == 0 || expiry == 0 || venue_ms < expiry {
                continue;
            }
            // A page naming the row twice is no record of its one
            // settlement (the parser review, round 2): which would hold?
            let twice = {
                let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
                let (sym, mut j, mut seen) = (x.symbol.get(body), 0, 0u32);
                // Stops at the second sight (the alloc review, round 3).
                while j < n && seen < 2 {
                    seen += u32::from(self.exercise[j].symbol.get(body) == sym);
                    j += 1;
                }
                seen > 1
            };
            let px = settle_px_1e6(self.wire.opt(row), self.wire.row(row).unit_1e6, x.exercise_px_1e6);
            if twice || !settlement_agrees(&x, booked, px) {
                self.scan_failed(SCAN_REST, ScanErr::BadField, now);
                continue;
            }
            let side = if booked > 0 { Side::Ask as u8 } else { Side::Bid as u8 };
            let ts = self.fill_ts_ns(x.time_ms, now);
            if !self.book_fill(None, None, row, slot, side, px, booked.saturating_abs(), 0, FILL_FLAG_SETTLEMENT, ts, now, 0) {
                // Lost (lane 4 and the held queue full): nothing booked —
                // the next read asks again.
                continue;
            }
            self.legs[i].booked_1e6 = 0;
            let w = self.wire.row(row);
            let asset = fee_asset(w, b"USDT");
            let (code, notional) = (fee_code(false, asset, TRADE_BY_SETTLEMENT, w), fee_notional_1e9(w, px, booked));
            // The record's fee ×1e6 → ×1e9, a charge whatever its sign.
            let fee_1e9 = x.fee_1e6.saturating_abs().saturating_mul(1_000);
            self.note(J_FEE, now, 0, 0, px, notional, fee_1e9, code, row, slot);
        }
    }

    /// While a spot order's trades are being read — queued, paused or in
    /// flight on lane `l` — a commission is owed that the read has not
    /// paid, and its asset is the read's to say (the base; BNB's row when
    /// the burn pays): every spot leg is held this cycle ([`Leg::held`];
    /// the BX8 e2e review — a status-booked fill read as drift for one
    /// cycle, and a BNB row's leg judged while its fee was owed). Derived
    /// from the queue each cycle, never a counter a lost job could leave
    /// set; a read given up leaves the queue, and the next cycle (asked for
    /// at once, [`Self::order_trades_done`]) judges the legs.
    fn hold_owed_legs(&mut self, l: usize) {
        let mut owed = matches!(self.lanes[l].job, Some(j) if j.kind == JobKind::OrderTrades);
        let mut k = 0;
        while !owed && k < self.lanes[l].jobs.len() {
            owed = matches!(self.lanes[l].jobs.get(k), Some(j) if j.kind == JobKind::OrderTrades);
            k += 1;
        }
        let mut k = 0;
        while owed && k < self.n_spot {
            if let Some(g) = self.legs.get_mut(self.spot_rows[k] as usize) {
                g.held = true;
            }
            k += 1;
        }
    }

    /// **BX-19 on a COIN-M lane's classic account read** (BX7): every owned
    /// row of the lane listed — read `gen` marked the rows it listed —
    /// cross-margined and at most `max_leverage` (`isolated`, `over`: the
    /// read's owned rows; the scan refused a position short of either
    /// field). Before the gateway is ready a breach refuses the boot. Once
    /// ready, an isolated row is a margin-type lock, and one above the cap
    /// — or an owned perpetual the read no longer lists (a dated contract
    /// leaves the list at its delivery) — a configuration lock, raised once.
    fn judge_cm_margin(&mut self, ln: usize, gen: u32, isolated: bool, over: bool, now: u64) {
        let ready = self.phase == Phase::Ready;
        let mut unlisted = false;
        let mut i = 0;
        while i < self.rows.len() {
            let w = self.wire.row(i as u16);
            let mine = w.flags & INST_OWNED != 0 && self.lane_ix[w.product as usize] as usize == ln;
            unlisted |= mine && self.rows[i].listed != gen && !(ready && w.flags & INST_DATED != 0);
            i += 1;
        }
        if !ready {
            let e = if isolated {
                AssertErr::Isolated
            } else if over {
                AssertErr::Leverage
            } else if unlisted {
                AssertErr::Unlisted
            } else {
                return;
            };
            self.refuse_boot(BootErr::Assert(e));
        } else if isolated {
            self.margin_lock(CODE_MARGIN_TYPE, now);
        } else if over || unlisted {
            self.margin_lock(CODE_CONFIG, now);
        }
    }

    /// A margin-type or leverage lock (BX7, BX-19): raised once a session.
    fn margin_lock(&mut self, code: i32, now: u64) {
        if !self.margin_locked {
            self.margin_locked = true;
            self.venue_lock(code, now);
        }
    }

    /// The venue's open orders, asked for at `sent_ns`: ours (listed),
    /// ghosts and orphans (cancelled), foreign; then the working orders
    /// the listing should have named and did not (in doubt), the sweep's
    /// confirmation and — unless only the listing was read — the verdict.
    fn orders_answered(&mut self, l: usize, status: u16, b0: usize, b1: usize, sent_ns: u64, now: u64) {
        let listing_only = self.recon_listing_only;
        if status != 200 {
            return self.recon_failed();
        }
        let n = {
            let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
            scan_orders(body, &mut self.orders[..])
        };
        let Ok(n) = n else {
            self.scan_failed(SCAN_REST, ScanErr::Malformed, now);
            return self.recon_failed();
        };
        let mut ix = 0u16;
        while (ix as usize) < OOT_CAP {
            if self.oot.is_open(ix) && self.lane_of_row(self.oot.get(ix).row) == l {
                self.oot.get_mut(ix).flags &= !OF_LISTED;
            }
            ix += 1;
        }
        let able = self.ready_to_send(l, now);
        let product = self.lanes[l].product;
        // The cycle's verdict so far (every lane before this one).
        let mut v = Verdict {
            shared: self.knobs.shared,
            ..self.verdict
        };
        let mut ours_open = 0u32;
        let mut i = 0;
        while i < n {
            let o = &self.orders[i];
            i += 1;
            let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
            let cid = o.cid.get(body);
            let row = self.wire.find(product, o.symbol.get(body));
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
            // BX8: no spot cancel is built (IoC-only, BX-17) — nor an options
            // one (BX9) — a stray there is counted, never cancelled: at boot
            // it refuses the boot by name (the operator cancels it in the
            // app; the risk review — never a cycle per second to the
            // timeout); a running arm stays unreconciled.
            let ioc_only = product == PRODUCT_SPOT || product == PRODUCT_OPTIONS;
            if stray && ioc_only && row != ROW_NONE && self.phase != Phase::Ready && self.boot_err.is_none() {
                // `refuse_boot`'s law, field by field (the listing is lent).
                self.boot_err = Some(if product == PRODUCT_SPOT { BootErr::SpotStray(row) } else { BootErr::OptStray(row) });
            }
            if !stray || row == ROW_NONE || ioc_only {
                continue;
            }
            let venue_oid = o.venue_oid;
            if let (true, Some(a)) = (able, self.active_ws(l)) {
                // Cancelled by the venue's own id, rendered straight from
                // the answer into the frame.
                if let Ok(id) = self.desk.issue(&self.stats, WsReq::on(REQ_CANCEL, IX_NONE, 0, 0), sess(l, a), now) {
                    let sym = self.wire.row(row).symbol();
                    let st = self.lanes[l].st;
                    if queue_cancel(&mut self.lanes[l].ws[a], id, &st, sym, Part::Lit(cid), self.clock.venue_ms(now)).is_err() {
                        let _ = self.desk.take(id);
                    } else {
                        self.stats.note_action_sent();
                    }
                }
            } else if able && self.lanes[l].route.is_some() {
                // The REST route (BX7): by the venue's order id — the
                // answer that holds the client id is overwritten before
                // the route's turn comes.
                let _ = self.post(l, WsReq::on(REQ_CANCEL, IX_NONE, u32::from(row), venue_oid), now);
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
            if self.oot.is_open(ix) && self.lane_of_row(self.oot.get(ix).row) == l {
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
        self.recon_ours += ours_open;
        self.verdict = v;
        // A cycle reads every lane before it judges (BX7, M3): the next
        // lane's account — or, for the sweep's listing alone, its listing.
        let next = l + 1;
        if next < self.lanes.len() {
            let more = if listing_only {
                self.recon_lane = next as u8;
                self.queue_job(next, JobKind::OpenOrders, 0)
            } else {
                self.recon_account(next, now, false)
            };
            if !more {
                self.recon_failed();
            }
            return;
        }
        self.recon = ReconStep::Idle;
        self.recon_listing_only = false;
        let ours = self.recon_ours;
        if self.sweep.active && now >= self.sweep.confirm_at_ns {
            self.sweep_confirm(ours, now);
        }
        if listing_only {
            return;
        }
        // Portfolio Margin and PM Pro: the unified account — the margin, the
        // equity, the account's status — is the cycle's last step, and the
        // verdict waits on it (BX7): a cycle that cannot read it fails, so a
        // margin and a status unread can never pass as reconciled.
        if self.pm || self.pm_pro {
            self.recon = ReconStep::Unified;
            if !self.queue_pm_account() {
                self.recon_failed();
            }
            return;
        }
        self.cycle_verdict(now);
    }

    /// **The cycle's verdict**, once every lane's account and listing was
    /// read — and, in Portfolio Margin and PM Pro, the unified account.
    fn cycle_verdict(&mut self, now: u64) {
        let mut v = self.verdict;
        self.verdict = Verdict::default();
        // A refused boot judges nothing more: nothing adopted, journalled or
        // pushed to lane 4 for a boot that never runs (the risk review's
        // N5; a refusal is only ever raised before Ready).
        if self.boot_err.is_some() {
            return;
        }
        // BX7: the boot's first full cycle adopts the venue's positions
        // (a dedicated account, within the cap) — before it judges them.
        if self.phase == Phase::Recon && !self.adopt_ran {
            if let Err(usd) = self.adopt(now) {
                return self.refuse_boot(BootErr::Inventory(usd));
            }
        }
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

    /// **Adopt the venue's positions as the owner slot's starting
    /// inventory** (BX7, the operator's ruling; module docs) — once, at the
    /// boot's first full cycle, on a dedicated account: every owned row the
    /// venue holds and this boot has not booked. Their exposure in all —
    /// a linear leg at the read's mark, an inverse one at its face — must
    /// fit [`GwKnobs::adopt_cap_usd_1e6`]; `Err` is that exposure
    /// (`i64::MAX` for a linear leg with no price), and nothing is booked.
    /// A LONG option at a mark of 0 is priced — worth nothing (its read
    /// requires the mark; the parser review); a short one is priced by its
    /// margin's bound or not at all.
    fn adopt(&mut self, now: u64) -> Result<(), i64> {
        self.adopt_ran = true;
        if self.knobs.shared {
            return Ok(());
        }
        // BX8: a spot balance under a millionth (dust) reads 0 at ×1e6 —
        // never adopted, never compared — yet a later fill brings it into
        // view: the explained balance starts at it, to its last decimal.
        let mut k = 0;
        while k < self.n_spot {
            let i = self.spot_rows[k] as usize;
            if self.legs[i].venue_1e6 == 0 && self.rows[i].spot_1e8 == 0 {
                self.rows[i].spot_1e8 = self.spot_bal_1e8[k];
            }
            k += 1;
        }
        let (mut rows, mut usd) = (0u32, 0i64);
        let mut i = 0;
        while i < self.legs.len() {
            if self.adoptable(i) {
                let px = self.adopt_px_1e6(i);
                let long_option = self.wire.row(i as u16).product == PRODUCT_OPTIONS && self.legs[i].venue_1e6 > 0;
                if px < 0 || (px == 0 && !long_option) {
                    return Err(i64::MAX);
                }
                rows += 1;
                let leg_usd = (self.legs[i].venue_1e6.unsigned_abs() as i128 * px as i128 / 1_000_000).min(i64::MAX as i128) as i64;
                usd = usd.saturating_add(leg_usd);
            }
            i += 1;
        }
        if rows == 0 {
            return Ok(());
        }
        // Over the cap — or more than the held queue could take should lane 4
        // be full: two records a row (its fill and its position event), and
        // the cycle's own two (its verdict, the anchor's margin sample) —
        // checked before anything is booked (all or nothing).
        if usd > self.knobs.adopt_cap_usd_1e6 || 2 * rows as usize + 2 > HELD_CAP - self.held.len() {
            return Err(usd);
        }
        let (owner, ts) = (self.knobs.owner_slot, self.fill_ts_ns(0, now));
        let mut i = 0;
        while i < self.legs.len() {
            if self.adoptable(i) {
                let row = i as u16;
                let venue = self.legs[i].venue_1e6;
                let side = if venue > 0 { Side::Bid as u8 } else { Side::Ask as u8 };
                // A linear leg at the read's mark; an inverse one is its
                // face's exposure whatever the price — it carries none.
                let px = if self.wire.row(row).flags & INST_INVERSE != 0 { 0 } else { self.legs[i].px_1e6 };
                if !self.book_fill(None, None, row, owner, side, px, venue.saturating_abs(), J_FILL_ADOPTED, FILL_FLAG_ADOPTED, ts, now, 0) {
                    // The room was checked: unreachable — fail closed all the same.
                    debug_assert!(false, "an adopted fill lost after the room check");
                    return Err(usd);
                }
                self.legs[i].booked_1e6 = venue;
                // BX8: a spot row's explained balance is the venue's own, to
                // its last decimal (the fill carried its ×1e6 part).
                let k = self.rows[i].spot_k;
                if k != u8::MAX {
                    self.rows[i].spot_1e8 = self.spot_bal_1e8[k as usize];
                }
            }
            i += 1;
        }
        self.adopted_rows = rows;
        self.adopted_usd_1e6 = usd;
        self.adopt_wait = true;
        Ok(())
    }

    /// **The price leg `i`'s exposure is adopted at** ×1e6 (BX7): its read's
    /// price — a linear leg's mark, an inverse one's face, a spot balance's
    /// ticker — and, a short option's (BX9), its initial margin's upper
    /// bound per contract: the mark plus 15 % of the underlying's index ×
    /// the unit ([`OPT_SHORT_IM_MAX_1E6`]; 0 — unpriced, refused — with no
    /// index read, or one the venue's own mark of the row disagrees with:
    /// the mark outside the no-arbitrage band at that index
    /// ([`core_types::option_mark_band`]; the risk review's R7 — an index
    /// read low would understate the bound).
    fn adopt_px_1e6(&self, i: usize) -> i64 {
        let g = &self.legs[i];
        let row = i as u16;
        let w = self.wire.row(row);
        if w.product != PRODUCT_OPTIONS || g.venue_1e6 >= 0 {
            return g.px_1e6;
        }
        let o = self.wire.opt(row);
        let uly = o.uly as usize;
        let index = if uly < OPT_ULY_MAX { self.opt_index_1e6[uly] } else { 0 };
        let (lo, hi) = core_types::option_mark_band(index, o.strike_1e6, w.unit_1e6, o.call);
        let m = i128::from(g.px_1e6);
        if index <= 0 || g.px_1e6 < 0 || m < lo || m > hi {
            return 0;
        }
        let im = index as i128 * OPT_SHORT_IM_MAX_1E6 as i128 / 1_000_000 * w.unit_1e6 as i128 / 1_000_000;
        (im + g.px_1e6 as i128).min(i64::MAX as i128) as i64
    }

    /// A leg the boot adopts (BX7): an owned row the venue holds a position
    /// on, this boot has not booked, and the arm can trade. A row that is
    /// not live (not trading, an inexact tick) is never adopted — the arm
    /// could not exit it: its leg stays unseen, the slot unreconciled until
    /// it is flattened by hand (fail closed). A leg this boot already
    /// booked is not adopted again: any difference is drift.
    #[inline]
    fn adoptable(&self, i: usize) -> bool {
        let l = &self.legs[i];
        l.owned && l.venue_1e6 != 0 && l.booked_1e6 == 0 && self.wire.row(i as u16).flags & INST_LIVE != 0
    }

    /// What the boot adopted (BX7): the rows, and their exposure USD ×1e6.
    #[must_use]
    pub const fn adopted(&self) -> (u32, i64) {
        (self.adopted_rows, self.adopted_usd_1e6)
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
        // The session's equity (USDⓈ-M's on a classic account, the unified
        // account's in PM and PM Pro), sampled on USDⓈ-M's lane.
        let um = self.lane_ix[PRODUCT_USDM as usize];
        let mut e = BnEvt::new(EVT_MARGIN, now);
        e.product = PRODUCT_USDM;
        e.a = if um == NO_LANE { 0 } else { self.lanes[um as usize].ratio_1e6 };
        e.b = self.equity_1e6;
        e.c = self.anchor_seen();
        self.emit(&e);
    }

    fn start_day(&mut self, now: u64) {
        let mut left = 0u16;
        let mut i = 0;
        while i < self.rows.len() {
            let w = self.wire.row(i as u16);
            let want = w.flags & INST_OWNED != 0 && w.flags & INST_LIVE != 0 && !self.rows[i].day_read;
            if want && !self.rows[i].day_queued && self.queue_job(self.lane_of_row(i as u16), JobKind::UserTrades, i as u16) {
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

    fn trades_answered(&mut self, l: usize, row: u16, status: u16, b0: usize, b1: usize, now: u64) {
        let n = if status == 200 {
            let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
            scan_trades(body, &mut self.trades[..]).ok()
        } else {
            None
        };
        let r = row as usize;
        match n {
            // A full page may be cut short: refused, never summed (S7-L1).
            Some(n) if n < TRADES_PAGE && r < self.rows.len() => {
                // Linear: the increase × its price; inverse (COIN-M): the
                // increase in contracts × the face (USD either way).
                let w = self.wire.row(row);
                let (inverse, face) = (w.flags & INST_INVERSE != 0, w.unit_1e6);
                let cur = self.legs[r].venue_1e6;
                // BX8: a spot row's day is its buys (`day_buys_1e6`).
                self.rows[r].day_spent_1e6 = if self.rows[r].spot_k != u8::MAX {
                    day_buys_1e6(&self.trades[..n])
                } else {
                    day_increasing_1e6(&mut self.trades[..n], cur, inverse, face)
                };
                self.rows[r].day_read = true;
            }
            _ => {}
        }
        self.day_left = self.day_left.saturating_sub(1);
        if self.day_left == 0 {
            self.day_done(now);
        }
    }

    /// **A spot order's trades** (BX8), read after a fill of it was booked
    /// without its report: each trade's commission not yet paid — the
    /// dedupe ring says which — is journaled (`J_FEE`) and paid out of the
    /// explained balance of the row whose base asset it was charged in (the
    /// module docs' balance law); the trade ids join the ring, so a report
    /// arriving late pays nothing again. The order id in the journal is the
    /// venue's (the member's may have ended). A trade of another order or
    /// symbol is no answer to this read: not paid; a trade naming no
    /// commission asset is no spot trade (a scan failure). Unscannable, the
    /// commissions stay unpaid: drift (fail closed).
    /// `false`: the answer names fewer of the order's trades than it
    /// executed (none, or a page short of them) — the caller asks again.
    fn order_trades_answered(&mut self, l: usize, j: Job, b0: usize, b1: usize, now: u64) -> bool {
        let (row, venue_oid) = (j.row, j.oid);
        if (row as usize) >= self.rows.len() {
            self.order_trades_done(now);
            return true;
        }
        let n = {
            let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
            scan_trades(body, &mut self.trades[..])
        };
        let Ok(n) = n else {
            self.scan_failed(SCAN_REST, ScanErr::Malformed, now);
            self.order_trades_done(now);
            return true;
        };
        let slot = self.knobs.owner_slot;
        // The order's trades the answer names, ×1e6.
        let mut named_1e6 = 0i64;
        let mut k = 0;
        while k < n {
            let t = self.trades[k];
            k += 1;
            let (ours, fee_row, asset) = {
                let body = self.lanes[l].rest.resp().get(b0..b1).unwrap_or(&[]);
                let a = t.fee_asset().get(body);
                let ours = t.venue_oid == venue_oid && t.symbol.get(body) == self.wire.row(row).symbol();
                (ours, self.spot_row_of(a), fee_asset(self.wire.row(row), a))
            };
            if ours && t.fee_asset().len == 0 {
                self.scan_failed(SCAN_REST, ScanErr::Missing, now);
                self.order_trades_done(now);
                return true;
            }
            if ours {
                named_1e6 = named_1e6.saturating_add(t.qty_1e6);
            }
            if !ours || !self.tids.see(row, t.trade_id, true).fee_due {
                continue;
            }
            let w = self.wire.row(row);
            let (code, notional) = (fee_code(t.maker != 0, asset, TRADE_BY_MEMBER, w), fee_notional_1e9(w, t.px_1e6, t.qty_1e6));
            self.note(J_FEE, now, 0, t.venue_oid, t.px_1e6, notional, t.commission_1e9, code, row, slot);
            if let Some(r) = self.rows.get_mut(fee_row as usize) {
                r.spot_1e8 = r.spot_1e8.saturating_sub(t.commission_1e9 / 10);
            }
        }
        // Short of the order's executed quantity: the venue's list lags
        // its match (R2-3) — the trades it named are paid (once: the ring),
        // the rest asked for again.
        if named_1e6 < j.qty_1e6 {
            return false;
        }
        self.order_trades_done(now);
        true
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
        // BX7: not before lane 4 has delivered the adopted inventory — the
        // report makes the arm `reconciled`, which seeds the slot.
        if all && !self.day_sent && !self.adopt_wait {
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
            let _ = self.queue_job(0, JobKind::Time, 0);
        }
        self.clock_next_ns = now + CLOCK_EVERY_NS;
    }

    fn timers(&mut self, now: u64) {
        // The deadline queue, every pass: it orders them exactly.
        self.deadlines(now);
        if now < self.timers_next_ns {
            return;
        }
        self.timers_next_ns = now + MS;
        // REST timeouts are the transport's.
        if let Some(s) = self.sapi.as_mut() {
            let p = s.on_tick(now);
            self.on_rest_progress(0, p, now, true);
        }
        let mut l = 0;
        while l < self.lanes.len() {
            self.lane_ticks(l, now);
            l += 1;
        }
        self.dns_tick();
        if self.phase == Phase::Connect && self.all_connected() {
            self.phase = Phase::Recon;
            self.recon_next_ns = now;
        }
        if self.phase == Phase::Ready || self.phase == Phase::Exiting {
            self.ready_timers(now);
        }
        // BX7: the adopted inventory delivered (lane 4 drained — the engine
        // books a fill before it frees its slot), the day may be reported.
        if self.adopt_wait && self.held.len() == 0 && self.fills.is_empty() {
            self.adopt_wait = false;
            self.day_done(now);
        }
        // The cadence moves only when a cycle started (N7: a "now" asked
        // for while one is in flight is kept, not lost).
        // The cycle at its cadence is the boot's weight model: it never
        // waits on the throttle (a configuration the boot accepted must run
        // — its margin and its bound sampled, its staleness judged). Only a
        // cycle asked for early (a stream's reopen) waits while the weight
        // is throttled — unless the boot runs or the sweep's confirmation
        // asked for it.
        let early = now < self.recon_cadence_ns && self.phase == Phase::Ready;
        let wanted = !early || !self.throttled() || (self.sweep.active && now >= self.sweep.confirm_at_ns);
        if matches!(self.phase, Phase::Recon | Phase::Day | Phase::Ready) && now >= self.recon_next_ns && wanted && self.start_recon(0, now, false) {
            self.recon_next_ns = now + self.knobs.recon_every_ms * MS;
            self.recon_cadence_ns = self.recon_next_ns;
        }
        let mut l = 0;
        while l < self.lanes.len() {
            self.start_jobs(l, now);
            l += 1;
        }
    }

    /// **DNS re-resolve** (BX7, [`crate::dns`]): every answer to every
    /// connection on its host (for its next dial); then each host's worst
    /// streak of failed establishments, asked again where due.
    fn dns_tick(&mut self) {
        while let Some(a) = self.dns.answer() {
            if let Some(addr) = a.addr {
                self.set_host_addr(a.host, addr);
                self.t[crate::arm::GW_DNS_RERESOLVED] += 1;
            }
        }
        let mut worst = [0u32; DNS_HOSTS];
        let mut l = 0;
        while l < self.lanes.len() {
            let ln = &self.lanes[l];
            let (w, u, r) = (usize::from(ln.hosts[0]), usize::from(ln.hosts[1]), usize::from(ln.hosts[2]));
            worst[w] = worst[w].max(ln.ws[0].fail_streak()).max(ln.ws[1].fail_streak());
            worst[u] = worst[u].max(ln.us.fail_streak());
            if let Some(k) = ln.risk.as_ref() {
                worst[u] = worst[u].max(k.ws.fail_streak());
            }
            worst[r] = worst[r].max(ln.rest.fail_streak());
            if let Some(rt) = ln.route.as_ref() {
                worst[r] = worst[r].max(rt.conn.fail_streak());
            }
            l += 1;
        }
        if let Some(s) = self.sapi.as_ref() {
            let h = usize::from(self.sapi_host);
            worst[h] = worst[h].max(s.fail_streak());
        }
        self.dns.ask_where_due(&worst);
    }

    /// Host `h`'s new address onto every connection on it.
    fn set_host_addr(&mut self, h: u8, addr: SocketAddr) {
        let mut l = 0;
        while l < self.lanes.len() {
            let ln = &mut self.lanes[l];
            if ln.hosts[0] == h {
                ln.ws[0].set_addr(addr);
                ln.ws[1].set_addr(addr);
            }
            if ln.hosts[1] == h {
                ln.us.set_addr(addr);
                if let Some(k) = ln.risk.as_mut() {
                    k.ws.set_addr(addr);
                }
            }
            if ln.hosts[2] == h {
                ln.rest.set_addr(addr);
                if let Some(rt) = ln.route.as_mut() {
                    rt.conn.set_addr(addr);
                }
            }
            l += 1;
        }
        if self.sapi_host == h {
            if let Some(s) = self.sapi.as_mut() {
                s.set_addr(addr);
            }
        }
    }

    /// Lane `l`'s sockets, each timer pass: the transports' ticks (their
    /// own timeouts), the order sessions' machine or the REST route, and the
    /// user stream's reconnect (none while quiet).
    fn lane_ticks(&mut self, l: usize, now: u64) {
        let p = self.lanes[l].rest.on_tick(now);
        self.on_rest_progress(l, p, now, false);
        let mut s = 0;
        while s < 2 {
            let p = self.lanes[l].ws[s].on_tick(now);
            self.on_ws_progress(l, s, p, now);
            s += 1;
        }
        let p = self.lanes[l].us.on_tick(now);
        self.on_us_progress(l, p, now);
        if self.lanes[l].route.is_none() {
            self.drive_mbb(l, now);
        }
        // The REST route (BX7, C5): its timeouts, then its next request.
        let p = match self.lanes[l].route.as_mut() {
            Some(rt) => rt.conn.on_tick(now),
            None => Progress::Waiting,
        };
        self.on_route_progress(l, p, now);
        self.pump_route(l, now);
        let connecting = matches!(self.phase, Phase::Connect | Phase::Recon | Phase::Day | Phase::Ready) && now >= self.quiet_until_ns;
        // PM Pro's risk stream: its ticks, and its (re)connect on the
        // lane's key.
        let keyed = self.lanes[l].user.phase == UsPhase::Keyed;
        let p = match self.lanes[l].risk.as_mut() {
            Some(r) => r.ws.on_tick(now),
            None => WsProgress::Waiting,
        };
        self.on_risk_progress(l, p, now);
        if let Some(r) = self.lanes[l].risk.as_mut() {
            if connecting && keyed && r.ws.is_down() && now >= r.retry_ns && r.ws.connect(self.poll.registry(), now).is_err() {
                r.retry_ns = now + r.backoff.next_delay_ns();
            }
        }
        let ln = &mut self.lanes[l];
        if connecting
            && ln.us.is_down()
            && ln.user.phase == UsPhase::Keyed
            && now >= ln.us_retry_ns
            && ln.us.connect(self.poll.registry(), now).is_err()
        {
            ln.us_retry_ns = now + ln.us_backoff.next_delay_ns();
        }
        if ln.us.is_open() {
            ln.us_down_ns = 0;
        }
    }

    /// Every lane's order route is up and its user stream open (the boot's
    /// sockets).
    fn all_connected(&self) -> bool {
        let mut l = 0;
        while l < self.lanes.len() {
            if !self.order_up(l) || (self.lanes[l].has_us && !self.lanes[l].us.is_open()) {
                return false;
            }
            l += 1;
        }
        true
    }

    /// Every lane holds a listenKey.
    fn all_keyed(&self) -> bool {
        let mut l = 0;
        while l < self.lanes.len() {
            if self.lanes[l].has_us && self.lanes[l].user.phase != UsPhase::Keyed {
                return false;
            }
            l += 1;
        }
        true
    }

    /// BX-19's boot assertions: per futures lane the one-way mode — and on
    /// USDⓈ-M classic the single-asset mode (dapi and papi have none; PM
    /// Pro's collateral is the program's) — then, once, the key's
    /// permissions and the account mode (SAPI) — and the options key's own
    /// when it is not the arm's (BX9). Not on Demo, which serves
    /// no SAPI: classic by construction (`new`; module docs). Spot's lane
    /// asserts nothing here (BX8): its account's permissions are judged at
    /// every account read — nor does options' (BX9): its account's
    /// `canTrade` and `reduceOnly` are. None to ask (a spot- or options-only
    /// Demo arm): held now.
    fn queue_asserts(&mut self) {
        self.asserts_left = if self.demo { 0 } else { 2 + u16::from(self.opt_own_key) };
        let mut l = 0;
        while l < self.lanes.len() {
            if matches!(self.lanes[l].product, PRODUCT_SPOT | PRODUCT_OPTIONS) {
                l += 1;
                continue;
            }
            self.asserts_left += 1;
            let _ = self.queue_job(l, JobKind::Dual, 0);
            if self.lanes[l].product == PRODUCT_USDM && !self.lanes[l].unified {
                self.asserts_left += 1;
                let _ = self.queue_job(l, JobKind::MultiAssets, 0);
            }
            // BX-19 (BX7): each owned USDⓈ-M row cross-margined, at or
            // under the leverage cap — asked per row (COIN-M's account read
            // carries both for every symbol; PM is cross by construction).
            if self.lanes[l].product == PRODUCT_USDM && !self.lanes[l].pm {
                let mut i = 0;
                while i < self.rows.len() {
                    let w = self.wire.row(i as u16);
                    if w.flags & INST_OWNED != 0 && w.product == PRODUCT_USDM {
                        self.asserts_left += 1;
                        let _ = self.queue_job(l, JobKind::SymbolConfig, i as u16);
                    }
                    i += 1;
                }
            }
            l += 1;
        }
        if !self.demo {
            let _ = self.queue_job(0, JobKind::Restrictions, 0);
            let _ = self.queue_job(0, JobKind::Portfolio, 0);
            if self.opt_own_key {
                let _ = self.queue_job(0, JobKind::RestrictionsOpt, 0);
            }
        }
        if self.asserts_left == 0 {
            self.asserts_held();
        }
    }

    /// Every boot assertion held: each lane holding a stream asks for its
    /// listenKey — or, none holding one (a spot-only arm, BX8), the sockets
    /// open now.
    fn asserts_held(&mut self) {
        self.phase = Phase::ListenKey;
        let mut k = 0;
        while k < self.lanes.len() {
            if self.lanes[k].has_us {
                let _ = self.queue_job(k, JobKind::ListenNew, 0);
            }
            k += 1;
        }
        if self.all_keyed() {
            self.phase = Phase::Connect;
        }
    }

    /// A futures lane is armed: the key's futures permission is asserted
    /// (BX8: a spot-only arm asks spot's alone).
    fn has_futures(&self) -> bool {
        self.lane_ix[PRODUCT_USDM as usize] != NO_LANE || self.lane_ix[PRODUCT_COINM as usize] != NO_LANE
    }

    /// Every deadline due: TTL cancels (BX-13) and owed cancels; WS API
    /// answers that never came (S2).
    fn deadlines(&mut self, now: u64) {
        let mut x = Expired {
            deadline_ns: 0,
            key: 0,
            kind: 0,
        };
        while self.desk.tq.pop_due(now, &mut x) {
            match x.kind {
                TK_ORDER => self.order_due(x.key, now),
                TK_WS_REQ => self.request_due(x.key, now),
                _ => debug_assert!(false, "an unknown deadline kind {}", x.kind),
            }
        }
    }

    /// An open order's deadline: its TTL's cancel, or an owed cancel's
    /// retry — whose, the owed bits say (a member's first).
    fn order_due(&mut self, key: u64, now: u64) {
        let ix = (key & 0xFFFF) as u16;
        if (ix as usize) >= OOT_CAP || !self.oot.is_open(ix) || u64::from(self.oot.aux(ix).gen) != key >> 16 {
            // Its order ended (every end disarms it: defensive).
            return;
        }
        self.order_tid[ix as usize] = None;
        let m = self.oot.get_mut(ix);
        m.flags &= !OF_TIMED;
        let owe = if m.owe & OWE_MEMBER != 0 { OWE_MEMBER } else { OWE_TTL };
        if m.flags & OF_CANCEL_SENT != 0 {
            m.owe = 0;
            return;
        }
        if self.send_cancel(ix, now, owe == OWE_TTL) {
            self.oot.get_mut(ix).owe = 0;
        } else {
            // Not sendable now: still owed.
            self.owe_cancel(ix, owe, now + CANCEL_RETRY_NS);
        }
    }

    /// S2: a WS API request unanswered for [`WS_REQ_TIMEOUT_NS`] means its
    /// session is dead whatever its socket says (a half-open path: no FIN,
    /// no RST) — every request on it is in doubt and the socket is dropped;
    /// the machine dials again.
    fn request_due(&mut self, id: u64, now: u64) {
        let Some(c) = self.desk.pending.get(id).map(|r| r.conn as usize) else {
            return;
        };
        self.stats.note_post_timeout();
        self.ws_failed(c / 2, c % 2, now);
    }

    fn ready_timers(&mut self, now: u64) {
        if now >= self.status_next_ns {
            self.status_next_ns = now + STATUS_EVERY_NS;
            self.pulse(now);
        }
        if now >= self.clock_next_ns {
            self.start_clock_round(now);
        }
        // Each lane: its listenKey's keep-alive, and whether its order route
        // can send now.
        let mut ready = [false; LANES];
        let mut l = 0;
        while l < self.lanes.len() {
            if self.lanes[l].user.keepalive_due(now) {
                self.lanes[l].user.kept_ns = now;
                let _ = self.queue_job(l, JobKind::ListenKeep, 0);
            }
            ready[l] = self.ready_to_send(l, now);
            l += 1;
        }
        // The dead-man (O-BX13): every row with a maker of ours resting,
        // every heartbeat, ONLY while its lane's order route can cancel (B1).
        let hb = self.knobs.heartbeat_ms * MS;
        let mut i = 0;
        while i < self.rows.len() {
            let r = &self.rows[i];
            if r.makers > 0 && !r.cd_queued && now >= r.cd_next_ns {
                let l = self.lane_of_row(i as u16);
                if ready[l] && self.queue_job(l, JobKind::Countdown, i as u16) {
                    let r = &mut self.rows[i];
                    r.cd_queued = true;
                    r.cd_next_ns = now + hb;
                }
            }
            i += 1;
        }
        // N1: until one countdown has answered, the proving request —
        // `countdownTime=0`, which sets no timer — on a row with no maker
        // of ours; a refused row hands over to the next.
        let p = self.cd_probe_row;
        if p != ROW_NONE && !self.cd_proven && !self.probe_queued && now >= self.probe_next_ns && self.rows[p as usize].makers == 0 {
            let l = self.lane_of_row(p);
            if ready[l] && self.queue_job(l, JobKind::Probe, p) {
                self.probe_queued = true;
                self.probe_next_ns = now + hb;
            }
        }
        // In-doubt orders: one status query each, once a second, on its
        // lane's route — a REST route only when it is idle (a round's
        // queries are answered before the next round queues more).
        if now >= self.doubt_next_ns && self.oot.in_doubt() > 0 {
            let mut open = [false; LANES];
            let mut any = false;
            let mut l = 0;
            while l < self.lanes.len() {
                let idle = self.lanes[l].route.as_ref().is_none_or(|rt| rt.wire.is_none() && rt.queue.len() == 0);
                open[l] = ready[l] && idle;
                any |= open[l];
                l += 1;
            }
            if any {
                self.doubt_next_ns = now + IN_DOUBT_EVERY_NS;
                let mut ix = 0u16;
                while (ix as usize) < OOT_CAP {
                    if self.oot.is_open(ix) && self.oot.get(ix).state == ST_IN_DOUBT {
                        let l = self.lane_of_row(self.oot.get(ix).row);
                        if open[l] {
                            let oid = self.oot.get(ix).cid_oid;
                            let gen = self.oot.aux(ix).gen;
                            // A refused post closes the lane for this round.
                            open[l] = self.post(l, WsReq::on(REQ_STATUS, ix, gen, oid), now).is_ok();
                        }
                    }
                    ix += 1;
                }
            }
        }
        // S2: a half-open order session answers nothing and says nothing.
        // While a maker of ours rests, a session silent for a heartbeat is
        // asked for the margin: its timeout drops the session, which stops
        // the dead-man (B1).
        let mut l = 0;
        while l < self.lanes.len() {
            if self.lanes[l].account_next_ns == 0 && self.route_silent(l, now, hb) && self.lane_has_makers(l) {
                let ln = &mut self.lanes[l];
                ln.account_next_ns = now.max(ln.account_read_ns + ACCOUNT_REREAD_NS);
            }
            l += 1;
        }
        // A margin re-read (an ACCOUNT_UPDATE, a MARGIN_CALL, the probe
        // above): the lane's account alone (S5), one at a time.
        // (A full cycle due now reads the accounts itself: none extra, N-e.)
        // Paced at `ACCOUNT_REREAD_NS`, it is the boot's weight model too:
        // never throttled (the half-open probe must go out, BX7).
        let mut l = 0;
        while l < self.lanes.len() {
            let due = self.lanes[l].account_next_ns;
            if due != 0 && now >= due && now < self.recon_next_ns {
                // PM's and PM Pro's margin is the unified account's.
                let started = if self.lanes[l].unified { self.queue_pm_account() } else { self.start_recon(l, now, true) };
                if started {
                    self.lanes[l].account_next_ns = 0;
                    break;
                }
            }
            l += 1;
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
        // The sweep's confirmation read: the whole cycle while every lane's
        // order route is up; the REST listings alone while one is not (B1).
        if self.sweep.active && now >= self.sweep.confirm_at_ns && self.recon == ReconStep::Idle {
            let mut all = true;
            let mut l = 0;
            while l < self.lanes.len() {
                all &= ready[l];
                l += 1;
            }
            if all {
                self.recon_next_ns = now;
            } else if now >= self.quiet_until_ns && self.queue_job(0, JobKind::OpenOrders, 0) {
                self.recon = ReconStep::Orders;
                self.recon_listing_only = true;
                self.recon_lane = 0;
                self.recon_ours = 0;
                self.verdict = Verdict::default();
            }
        }
        // The 23 h rotation, make-before-break: the new session logs on
        // while this one carries posts (XG-4).
        let mut l = 0;
        while l < self.lanes.len() {
            let ln = &mut self.lanes[l];
            if let Some(a) = ln.mbb.active() {
                if now.saturating_sub(ln.opened_ns[a as usize]) >= self.knobs.rotate_after_ns && !ln.mbb.replace_pending() {
                    ln.mbb.request_replace();
                }
            }
            l += 1;
        }
    }

    /// A maker of ours rests on one of lane `l`'s rows.
    fn lane_has_makers(&self, l: usize) -> bool {
        if self.makers == 0 {
            return false;
        }
        let mut i = 0;
        while i < self.rows.len() {
            if self.rows[i].makers > 0 && self.lane_of_row(i as u16) == l {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Lane `l`'s order route has delivered nothing for `hb` (S2).
    #[inline]
    fn route_silent(&self, l: usize, now: u64, hb: u64) -> bool {
        let ln = &self.lanes[l];
        match &ln.route {
            Some(rt) => now.saturating_sub(rt.rx_ns) >= hb,
            None => ln.mbb.active().is_some_and(|a| now.saturating_sub(ln.rx_ns[a as usize]) >= hb),
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
        // Fail-closed across lanes: the longest gap, and up only while every
        // lane is.
        let mut gap = 0u64;
        let mut order_up = true;
        let mut user_up = true;
        let mut rest_live = 0usize;
        let mut l = 0;
        while l < self.lanes.len() {
            let ln = &self.lanes[l];
            let ord_gap = if ln.ord_down_ns == 0 { 0 } else { now.saturating_sub(ln.ord_down_ns) };
            let us_gap = if ln.us_down_ns == 0 || !ln.has_us { 0 } else { now.saturating_sub(ln.us_down_ns) };
            gap = gap.max(ord_gap).max(us_gap);
            order_up &= self.order_up(l);
            user_up &= !ln.has_us || ln.us.is_open();
            rest_live += ln.route.as_ref().map_or(0, |rt| rt.queue.len() + usize::from(rt.wire.is_some()));
            l += 1;
        }
        let mut e = BnEvt::new(EVT_STATUS, now);
        e.a = gap as i64;
        e.b = self.clock.offset_ms();
        e.c = self.oot.in_doubt() as i64;
        // DEADMAN_OK: a maker placed now is covered — the session can
        // cancel (B1) and every resting maker's countdown is current.
        let deadman = self.all_ready(now) && self.deadman_covers(now);
        // F2: the E7 anchor is not persisted yet — the ring has not taken
        // it, or the writer could not store it (a restart would re-anchor).
        let unsaved = self.anchor_pending || self.knobs.anchor_unsaved.as_ref().is_some_and(|f| f.load(Ordering::Relaxed));
        e.flags = (order_up as u8 * EVT_F_ORDER_UP)
            | (user_up as u8 * EVT_F_USER_UP)
            | (deadman as u8 * EVT_F_DEADMAN_OK)
            | (self.clock.measured() as u8 * EVT_F_CLOCK_OK)
            | (self.lost as u8 * EVT_F_LOST)
            | (unsaved as u8 * EVT_F_ANCHOR_UNSAVED);
        // The stats block first — the tallies the arm reads on this pulse,
        // the gauges and the heartbeat (at most ten stores a second on the
        // lines the engine reads).
        self.t[crate::arm::GW_JOURNAL_DROPPED] = self.journal.dropped();
        let mut i = 0;
        while i < crate::arm::GW_TALLIES {
            self.stats.publish(i, self.t[i]);
            i += 1;
        }
        self.stats.set_gauges((self.desk.pending.live() + rest_live) as u64, order_up);
        // The weight windows move with the clock; their words with the beat.
        self.weight.advance(now);
        self.opt_weight.advance(now);
        let level = if self.weight.at_least(ARM_WEIGHT_MAX_PER_MIN, 100) || self.opt_weight.at_least(OPT_ARM_WEIGHT_MAX_PER_MIN, 100) {
            CONGESTION_RATE_LIMITED
        } else if self.throttled() {
            CONGESTION_THROTTLED
        } else {
            CONGESTION_NONE
        };
        self.stats.set_congestion(level);
        self.stats.publish(PW_WEIGHT, self.weight.sum());
        self.stats.publish(PW_OPT_WEIGHT, self.opt_weight.sum());
        self.stats.beat(now);
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

/// **An expired option's settlement price per contract** ×1e6 (BX9): its
/// intrinsic value at the settlement price `s_1e6` — `max(S − K, 0)` for a
/// call, `max(K − S, 0)` for a put, per underlying unit — times the
/// contract's `unit_1e6`; saturating, never negative.
#[must_use]
pub fn settle_px_1e6(o: &OptWire, unit_1e6: i64, s_1e6: i64) -> i64 {
    let (s, k) = (i128::from(s_1e6), i128::from(o.strike_1e6));
    let d = if o.call { s - k } else { k - s };
    if d <= 0 {
        return 0;
    }
    (d * i128::from(unit_1e6) / 1_000_000).min(i128::from(i64::MAX)) as i64
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
