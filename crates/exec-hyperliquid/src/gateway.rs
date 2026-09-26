// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The Hyperliquid order gateway (XH4b): one thread per HL arm, and the
//! only place an HL order meets a socket.
//!
//! ```text
//! engine ──HlCmd ring──▶ gateway ──WS post──▶ venue
//!    ▲                     │  coalesce ALO │ taker │ modify │ cancel, ≤ 39 per action; control alone
//!    └──ExecEvent ring─────┘  msgpack → sign → envelope → frame, in the TX buffer
//! ```
//!
//! **The turn.** Poll both connections; read every frame in place
//! (post answers, `orderUpdates`, `userFills`, pongs); dial or retire per
//! the make-before-break machine (XG-4); pop commands until the ring is
//! empty — no artificial wait — while fewer than [`MAX_IN_FLIGHT`] posts
//! are out and the event ring has room for every in-flight item's answer
//! (XG-2); post each open batch; expire deadlines; keep alive. Idle turns
//! spin, then park up to the idle policy's bound (deviation 11: no
//! doorbell).
//!
//! **Admission.** A command is refused unsent ([`EV_REJECTED`]) when its
//! slot is not one this gateway serves, its kind or time-in-force is not
//! one it carries (ALO and IoC places, ALO modifies, cancels, and the
//! control actions `updateLeverage`, `scheduleCancel`, `noop`), the halt
//! word is set (growth only — a place, a modify, a leverage change —
//! [`REASON_HALTED`]; cancels, `scheduleCancel` and `noop` still go), or
//! — growth only — the process's first `userFills` snapshot has not been
//! absorbed yet ([`REASON_NOT_READY`]): a fill of ours can never be
//! mistaken for history.
//!
//! **Coalescing (XH-3).** Five classes, never mixed in one action: ALO
//! places, IoC places, ALO modifies (LAW E-7), cancels by cloid — at most
//! [`MAX_ORDERS`] items (39: one IP request) — and control actions, one
//! per post, each after every batch opened before it. Batches post in the
//! order they opened; a full batch posts every open batch; a cancel or
//! modify whose target is still in an open batch posts every open batch
//! first.
//! On ONE socket, therefore, nothing overtakes the order it names. Across
//! sockets (a make-before-break swap, the REST fallback) and after an
//! unanswered post there is no such guarantee, and the venue documents no
//! execution order between posts: XH4d's sticky intent covers cancels AND
//! modifies (a not-found answer about an unresolved target does not mean
//! the order is gone).
//!
//! **One post.** The action's msgpack is encoded once and signed (LAW
//! E-3; one nonce stream, on this thread only); its JSON is rendered once,
//! directly behind the WebSocket post head in the TX buffer
//! (`{"method":"post","id":N,"request":{"type":"action","payload":<envelope>}}`),
//! and framed in place behind [`WS_CLIENT_HDR_MAX`] bytes of room. A post
//! claims its pending slot when its batch opens (the items' references
//! are written there as they arrive) and its deadline before it is
//! written (XG-3: a growing post that cannot time out is not sent).
//!
//! **Answers (LAW E-5).** A reply's i-th status answers the post's i-th
//! item: `resting`/`filled` (a place or modify) and `success` (a cancel)
//! → [`EV_ACK`], `error` → [`EV_REJECTED`] with the reason the venue's
//! words name. A refused action (`status: err`) rejects every item. **In
//! doubt — [`EV_UNANSWERED`], never a rejection, never re-signed:** a
//! post past its deadline, an answer that does not scan, miscounts its
//! items or answers an item with a status its kind cannot have, a post
//! stranded on a socket that died or was retired, a WebSocket
//! `"type":"error"` answer (the venue's API layer, not the exchange; its
//! message's reason is carried), a REST answer that is not 2xx (429 on a
//! first send is a rejection), and ANY refusal of a re-send (below).
//!
//! **Fallback.** With no live connection a post goes to `/exchange` by
//! REST — the SAME envelope, a sub-slice of the rendered post. A post
//! whose socket died under the write is re-sent by REST as the same
//! signed bytes (the mask XORs back); the original may have landed, so
//! only a per-item answer settles it. The REST post blocks this thread
//! for up to its deadline — the fallback's cost, never the engine's.
//!
//! **Streams.** `userFills` → [`EV_FILL`] + [`EV_FILL_META`] (fee,
//! crossed), deduplicated by tid across connections and reconnects (LAW
//! E-5). The process's FIRST snapshot is history: its tids are admitted,
//! nothing is emitted, and [`EV_SNAPSHOT`] (rows, the latest fill's venue
//! time) tells the arm to read its boot reconciliation after it.
//! `orderUpdates` → status events, deduplicated (both sockets carry them
//! during a swap). A frame that does not scan, a socket that comes up
//! after a gap with no live socket, and one that replaces a socket that
//! had gone silent are [`EV_GAP`]: `orderUpdates` has no snapshot, so the
//! arm re-checks its open orders.
//!
//! **Copies** (house law): the msgpack encoding and the JSON rendering
//! (transforms, in place); the 64 B command out of its ring slot and each
//! 64 B event into its slot; the ≤ 125 B pong echo (rx → control
//! buffer); boot-time moves (the dedupe rings, the batch arrays, the coin
//! table); and core-net's transport copies — kernel ↔ rustls, rustls ↔
//! the receive buffer and its compaction. **Allocations:** none of the
//! gateway's own after construction; rustls 0.23's buffered API allocates
//! one buffer per TLS record it seals or opens — more for an inbound
//! burst over 4 KiB (its receive window grows and shrinks), and once per
//! connection when its decrypted-record queue first grows to its
//! high-water mark; gate 84b pins the residue exactly — and a dial or DNS
//! re-resolve allocates.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use core_net::{
    ws_frame_in_place, ws_unmask_in_place, Backoff, KeepaliveCfg, TlsTransport, WsOpcode,
    WS_CLIENT_HDR_MAX,
};
use core_time::now_ns;
use core_types::ORDER_EVENT_REASON_NONE;
use exec_gateway::stats::{HALT_TIMER_FULL, SWEEP_CLEAR, SWEEP_STRANDED, SWEEP_WORKING};
use exec_gateway::{
    boxed_array, duplex, CmdRx, EngineEnd, EvtTx, Expired, GatewayEnd, GatewayStats, Idle, Mbb,
    MbbAction, Pending, SlotState, TimerId, TimerQueue, HALT_NONE,
};
use mio::{Events, Poll};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

use crate::action::{
    encode_batch_modify, encode_cancel_by_cloid, encode_noop, encode_order, encode_schedule_cancel,
    encode_update_leverage, CancelByCloidWire, ModifyWire, OrderWire, Tif, MAX_ACTION, MAX_ORDERS,
};
use crate::cloid::{decode, Owner};
use crate::cmd::{
    HlCmd, CMD_CANCEL, CMD_FLAG_CROSS, CMD_LEVERAGE, CMD_MODIFY, CMD_NOOP, CMD_PLACE,
    CMD_READ_ORDERS, CMD_SCHEDULE_CANCEL,
};
use crate::event::{
    reason_of_error, ExecEvent, ASSET_UNKNOWN, EVF_BUY, EVF_CMD_SHIFT, EVF_CROSSED, EVF_FILLED,
    EVF_POST_ITEM, EVF_RESTING, EVF_SNAPSHOT, EV_ACK, EV_FILL, EV_FILL_META, EV_GAP, EV_HALT,
    EV_OPEN_ORDER, EV_POSITION, EV_READ_DONE, EV_READ_FAILED, EV_REJECTED, EV_SNAPSHOT,
    EV_UNANSWERED, GAP_FILLS_UNREADABLE, GAP_RECONNECT, GAP_UPDATES_UNREADABLE, READ_ORDERS,
    READ_STATE, REASON_HALTED, REASON_LOCAL, REASON_MALFORMED, REASON_NOT_READY, REASON_RATE_LIMIT,
    REASON_SETTLEMENT, REASON_TRANSPORT, SLOT_FOREIGN,
};
use crate::gwconn::{GwConn, Phase};
use crate::http::HlHttp;
use crate::json::{array_start, object_field, u64_field};
use crate::msgpack::MsgPackErr;
use crate::nonce::{now_ms, Nonce};
use crate::orderupd::{scan_order_updates, OrderUpdate};
use crate::recon::{
    open_orders_request, perp_state_request, scan_open_orders, scan_perp_state, OpenOrder,
    PerpPosition, MAX_OPEN_ORDERS,
};
use crate::request::{
    batch_modify_json, cancel_by_cloid_json, envelope_close, envelope_open, noop_json, order_json,
    schedule_cancel_json, update_leverage_json, ws_info_open, ws_post_close, ws_post_open,
};
use crate::response::{
    scan, scan_items, HlResponse, ItemStatus, ItemsReply, ITEM_ERROR, ITEM_FILLED, ITEM_RESTING,
    ITEM_SUCCESS,
};
use crate::sign::{sign_action, Network, Vault};
use crate::userws::{scan_user_fills, TidRing, UserFill, SNAPSHOT_RING};
use crate::wspost::{classify, PostKind, WsIn};

/// Command ring slots (engine → gateway).
pub const CMD_RING: usize = 1024;
/// Event ring slots (gateway → engine): four command rings (XG-2).
pub const EVT_RING: usize = 4096;
/// The engine's end of an HL gateway.
pub type HlEngineEnd = EngineEnd<HlCmd, ExecEvent, CMD_RING, EVT_RING>;
/// The gateway's end.
pub type HlGatewayEnd = GatewayEnd<HlCmd, ExecEvent, CMD_RING, EVT_RING>;

/// Build the duplex an arm and its gateway share. Boot-time.
#[must_use]
pub fn hl_duplex() -> (HlEngineEnd, HlGatewayEnd) {
    duplex::<HlCmd, ExecEvent, CMD_RING, EVT_RING>()
}

/// Posts in flight at most — the venue's WebSocket bound (per IP: once
/// several arms run gateways they share it, XH4f/XH4h).
pub const MAX_IN_FLIGHT: usize = 100;
/// How long a post waits for its answer before it is unanswered.
pub const POST_TIMEOUT_NS: u64 = 5_000_000_000;

const PENDING_SLOTS: usize = 256;
const _: () = assert!(PENDING_SLOTS >= MAX_IN_FLIGHT + CLASSES);
const TIMER_SLOTS: usize = 512;
const _: () = assert!(TIMER_SLOTS >= PENDING_SLOTS);
/// The TX buffer: a 39-item modify batch renders to ≈ 7 KiB.
const TX_CAP: usize = 16 * 1024;
const ROOM: usize = WS_CLIENT_HDR_MAX;
const GROUPING: &[u8] = b"na";

const CLASSES: usize = 6;
const CLASS_ALO: usize = 0;
const CLASS_TAKER: usize = 1;
const CLASS_MODIFY: usize = 2;
const CLASS_CANCEL: usize = 3;
/// One control action (leverage, scheduleCancel, noop), posted alone.
const CLASS_CONTROL: usize = 4;
/// One read (open orders, the perp sheet), posted alone, unsigned,
/// WebSocket only (XH4d).
const CLASS_READ: usize = 5;
/// Perp positions one state read may carry.
const MAX_POSITIONS: usize = 64;
/// XG-2: the event room a read's answer needs (its rows and its end).
const READ_RESERVE: usize = MAX_OPEN_ORDERS + 1;
const _: () = assert!(MAX_POSITIONS <= MAX_OPEN_ORDERS);
/// The slot byte of the gateway's own sweep read: never a strategy slot,
/// never `SLOT_FOREIGN`; its rows stay inside the gateway.
const SWEEP_SLOT: u8 = 0xFE;
/// Sweep rounds (read, cancel ours, settle) before the sweep is stranded.
const SWEEP_ROUNDS: u8 = 8;
// The sweep's table indexes by mask.
const _: () = assert!(MAX_OPEN_ORDERS.is_power_of_two());
/// A round's settle before the re-read, and a failed read's retry.
const SWEEP_SETTLE_NS: u64 = 1_000_000_000;
/// The heartbeat's period: the gateway stamps its stats block this often
/// while it turns, so the arm can tell a live socket from a dead thread
/// whose `ws_up` gauge froze at 1 (at most ten stores a second: the line
/// stays the gateway's).
const BEAT_NS: u64 = 100_000_000;
/// The sweep machine's phases.
const SW_IDLE: u8 = 0;
const SW_READ: u8 = 1;
const SW_WAIT: u8 = 2;
const SW_SETTLE: u8 = 3;
/// The sweep read's outcome, as the router hands it over.
const SWEEP_NONE: u8 = 0;
const SWEEP_READ: u8 = 1;
const SWEEP_FAILED: u8 = 2;

/// `PostRec::conn` of a post that went by REST.
const CONN_REST: u8 = 2;
/// `PostRec::conn` of a batch still being built.
const CONN_OPEN: u8 = 3;
const TIMER_POST: u8 = 1;

/// `orderUpdates` rows one frame may carry.
const MAX_UPDATES: usize = 256;
/// Recently seen `orderUpdates` rows (the dedupe window).
const UPDATE_RING: usize = 4096;
/// Event-ring room kept for the streams beyond the in-flight answers.
const STREAM_RESERVE: usize = 512;
/// How long an emit waits for the engine to free a slot before the drop
/// is counted and the arm halts (XG-2).
const ROOM_WAIT_NS: u64 = 50_000_000;

/// Ping after 30 s of silence; dead after 60 s (the venue cuts idle
/// sockets at 60 s).
const KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 30_000_000_000,
    idle_timeout_ns: 60_000_000_000,
};
/// An active connection silent this long (its ping unanswered for 15 s)
/// is replaced make-before-break while it still carries posts.
const REPLACE_QUIET_NS: u64 = 45_000_000_000;
/// XG-4: the backoff resets after a session up this long that moved data.
const HEALTHY_NS: u64 = 30_000_000_000;
const DIAL_BUDGET_NS: u64 = 10_000_000_000;
const DRAIN_BUDGET_NS: u64 = POST_TIMEOUT_NS + 1_000_000_000;
/// Consecutive failed dials before the host is re-resolved (a
/// CDN-fronted venue rotates addresses) — only while no socket is live.
const RERESOLVE_AFTER: u32 = 3;

/// Boot configuration.
pub struct GatewayCfg {
    /// API host (`api.hyperliquid[-testnet].xyz`, or a loopback).
    pub host: String,
    /// TLS port.
    pub port: u16,
    /// rustls client configuration.
    pub tls: Arc<ClientConfig>,
    /// The network the signatures are for.
    pub network: Network,
    /// The MASTER address the user channels are subscribed for.
    pub master: [u8; 20],
    /// The strategy slots this gateway posts for, as a bit mask (bit `s`
    /// = slot `s`); a command for any other slot is refused unsent — two
    /// arms can share an account, never a cloid namespace.
    pub slots: u8,
    /// A post's answer deadline ([`POST_TIMEOUT_NS`] in production).
    pub post_timeout_ns: u64,
    /// The idle policy.
    pub idle: Idle,
}

/// Why the gateway could not be built.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GatewayErr {
    /// The host did not resolve.
    Dns,
    /// The host is not a valid TLS server name.
    BadServerName,
    /// The poller could not be created.
    Poll,
    /// The REST fallback client could not be built.
    Http,
}

/// Coins the gateway resolves to asset ids for the streams (perps: the
/// arm's `/info meta` binding, XH4c). An unknown coin is
/// [`ASSET_UNKNOWN`]; the arm still has the client id.
#[derive(Clone)]
pub struct CoinMap {
    names: [[u8; COIN_MAX]; COIN_SLOTS],
    lens: [u8; COIN_SLOTS],
    assets: [u32; COIN_SLOTS],
    n: usize,
}

/// Coins a map holds.
pub const COIN_SLOTS: usize = 64;
/// Longest coin name.
pub const COIN_MAX: usize = 16;

impl Default for CoinMap {
    fn default() -> Self {
        Self::new()
    }
}

impl CoinMap {
    /// Empty.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            names: [[0u8; COIN_MAX]; COIN_SLOTS],
            lens: [0u8; COIN_SLOTS],
            assets: [0u32; COIN_SLOTS],
            n: 0,
        }
    }

    /// Bind `coin` to `asset`. `false`: the map is full or the name too
    /// long (boot refuses).
    pub fn insert(&mut self, coin: &[u8], asset: u32) -> bool {
        if self.n == COIN_SLOTS || coin.is_empty() || coin.len() > COIN_MAX {
            return false;
        }
        // COPY: ≤ 16 B coin name into the table, once per coin at boot —
        // the table outlives the /info reply it came from.
        self.names[self.n][..coin.len()].copy_from_slice(coin);
        self.lens[self.n] = coin.len() as u8;
        self.assets[self.n] = asset;
        self.n += 1;
        true
    }

    /// The asset `coin` is bound to, or [`ASSET_UNKNOWN`].
    #[must_use]
    pub fn asset(&self, coin: &[u8]) -> u32 {
        let mut i = 0usize;
        while i < self.n {
            let len = self.lens[i] as usize;
            if len == coin.len() && &self.names[i][..len] == coin {
                return self.assets[i];
            }
            i += 1;
        }
        ASSET_UNKNOWN
    }
}

/// The gateway's own counters (the stats granules carry the shared
/// ones; `/metrics` publishes these from XH4f).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GatewayCounters {
    /// Fills emitted.
    pub fills: u64,
    /// `userFills` frames that did not scan.
    pub fills_scan_failed: u64,
    /// Rows of the first snapshot, absorbed as history.
    pub snapshot_rows: u64,
    /// `orderUpdates` rows emitted.
    pub updates: u64,
    /// `orderUpdates` rows with a status the gateway does not report.
    pub updates_ignored: u64,
    /// `orderUpdates` frames that did not scan.
    pub updates_scan_failed: u64,
    /// Post answers that could not be attributed (their posts time out).
    pub answers_unreadable: u64,
    /// Answers that miscount their items or answer one with a status its
    /// kind cannot have.
    pub answers_mismatched: u64,
    /// `{"channel":"error"}` frames.
    pub ws_errors: u64,
    /// Commands refused before the socket.
    pub local_refusals: u64,
    /// Posts that cannot grow — cancels, `scheduleCancel`, `noop` — sent
    /// without a deadline (XG-3).
    pub cancels_untimed: u64,
    /// Dials started.
    pub dials: u64,
    /// Connections that went down.
    pub conn_drops: u64,
    /// Reads answered (XH4d).
    pub reads: u64,
    /// Reads whose answer did not scan (XH4d).
    pub reads_failed: u64,
    /// Sweep rounds run (XH4d).
    pub sweep_rounds: u64,
    /// Cancels a sweep sent (XH4d).
    pub sweep_cancels: u64,
    /// Resting orders of ours a sweep could not cancel: their coin is not
    /// in the coin map (XH4d).
    pub sweep_unmapped: u64,
}

/// One resting order of ours a sweep read found: what its cancel takes.
#[derive(Copy, Clone, Default)]
struct SweepHit {
    client_oid: u64,
    asset: u32,
    slot: u8,
}

/// One item of a post: what its answer events carry.
#[derive(Copy, Clone)]
struct ItemRef {
    client_oid: u64,
    prev: u64,
    ts_ns: u64,
    asset: u32,
    slot: u8,
    kind: u8,
    is_buy: u8,
}

const BLANK_ITEM: ItemRef = ItemRef {
    client_oid: 0,
    prev: 0,
    ts_ns: 0,
    asset: 0,
    slot: 0,
    kind: 0,
    is_buy: 0,
};

impl ItemRef {
    #[inline(always)]
    const fn of(c: &HlCmd) -> Self {
        Self {
            client_oid: c.client_oid,
            prev: c.prev_client_oid,
            ts_ns: c.ts_ns,
            asset: c.asset,
            slot: c.slot,
            kind: c.kind,
            is_buy: c.is_buy,
        }
    }

    /// The event every fact about this item starts from.
    #[inline(always)]
    fn event(&self, kind: u8, reason: u8, id: u64, now_ns: u64) -> ExecEvent {
        let side = if self.is_buy != 0 { EVF_BUY } else { 0 };
        ExecEvent {
            ts_ns: now_ns,
            client_oid: self.client_oid,
            aux: id,
            asset: self.asset,
            kind,
            reason,
            slot: self.slot,
            flags: EVF_POST_ITEM | (cmd_bits(self.kind) << EVF_CMD_SHIFT) | side,
            ..ExecEvent::default()
        }
    }
}

/// One post in flight (or a batch being built): its items, in order.
#[derive(Copy, Clone)]
struct PostRec {
    written_ns: u64,
    timer: Option<TimerId>,
    n: u8,
    class: u8,
    conn: u8,
    items: [ItemRef; MAX_ORDERS],
}

const BLANK_POST: PostRec = PostRec {
    written_ns: 0,
    timer: None,
    n: 0,
    class: 0,
    conn: CONN_OPEN,
    items: [BLANK_ITEM; MAX_ORDERS],
};

/// An open batch: its post id, open order, items, and the smallest
/// venue-expiry offset among them.
#[derive(Copy, Clone, Default)]
struct BatchHead {
    id: u64,
    seq: u64,
    n: usize,
    min_exp: u16,
}

/// A control action's arguments, resolved once at admission.
#[derive(Copy, Clone, Default)]
struct ControlWire {
    kind: u8,
    is_cross: bool,
    asset: u32,
    leverage: u32,
    /// `scheduleCancel`'s time; `None` clears.
    time: Option<u64>,
}

/// A control action into `dst`: its msgpack, or (`json`) its JSON twin —
/// one dispatch for both, so the signed bytes and the sent bytes can never
/// name different actions. A kind with no action is refused (the post is
/// refused locally; `class_of` admits none), never sent as a `noop`.
fn encode_control(c: &ControlWire, dst: &mut [u8], json: bool) -> Result<usize, MsgPackErr> {
    match (c.kind, json) {
        (CMD_LEVERAGE, false) => encode_update_leverage(dst, c.asset, c.is_cross, c.leverage),
        (CMD_LEVERAGE, true) => update_leverage_json(dst, c.asset, c.is_cross, c.leverage),
        (CMD_SCHEDULE_CANCEL, false) => encode_schedule_cancel(dst, c.time),
        (CMD_SCHEDULE_CANCEL, true) => schedule_cancel_json(dst, c.time),
        (CMD_NOOP, false) => encode_noop(dst),
        (CMD_NOOP, true) => noop_json(dst),
        // Not a buffer overflow: the refusal `seal` needs, before signing.
        _ => Err(MsgPackErr::Overflow),
    }
}

/// The class batches' items, as the encoders take them.
struct Batches {
    head: [BatchHead; CLASSES],
    alo: [OrderWire; MAX_ORDERS],
    taker: [OrderWire; MAX_ORDERS],
    modify: [ModifyWire; MAX_ORDERS],
    cancel: [CancelByCloidWire; MAX_ORDERS],
    control: ControlWire,
}

const BLANK_ORDER: OrderWire = OrderWire::new(0, false, 0, 0, Tif::Gtc);

impl Batches {
    fn new() -> Self {
        Self {
            head: [BatchHead::default(); CLASSES],
            alo: [BLANK_ORDER; MAX_ORDERS],
            taker: [BLANK_ORDER; MAX_ORDERS],
            modify: [ModifyWire {
                order: BLANK_ORDER,
                oid: 0,
                oid_cloid: [0u8; 16],
                oid_is_cloid: false,
            }; MAX_ORDERS],
            cancel: [CancelByCloidWire {
                asset: 0,
                cloid: [0u8; 16],
            }; MAX_ORDERS],
            control: ControlWire::default(),
        }
    }
}

/// Where a sealed post sits in the TX buffer.
#[derive(Copy, Clone)]
struct Sealed {
    /// The envelope (the REST body): `tx[env..env + env_len]`.
    env: usize,
    env_len: usize,
    /// The WebSocket post: `tx[ROOM..ROOM + ws_len]`.
    ws_len: usize,
}

/// A coalescing class, or `None` for a command the gateway does not
/// carry: a Gtc place (neither ALO nor IoC, XH-3), a taker modify, an
/// unknown kind or time-in-force.
#[inline(always)]
fn class_of(c: &HlCmd) -> Option<usize> {
    const ALO: u8 = Tif::Alo as u8;
    const IOC: u8 = Tif::Ioc as u8;
    match (c.kind, c.tif) {
        (CMD_CANCEL, _) => Some(CLASS_CANCEL),
        (CMD_MODIFY, ALO) => Some(CLASS_MODIFY),
        (CMD_PLACE, ALO) => Some(CLASS_ALO),
        (CMD_PLACE, IOC) => Some(CLASS_TAKER),
        _ if c.is_control() => Some(CLASS_CONTROL),
        _ if c.is_read() => Some(CLASS_READ),
        _ => None,
    }
}

/// A command that can add exposure: a place, a modify or a leverage
/// change — a reduce-only place included (XH1: no member relies on the
/// bit for an exit). The halt and the snapshot gate refuse it, and XG-3
/// will not send it untimed; cancels, `scheduleCancel` and `noop` always
/// go.
#[inline(always)]
const fn grows(kind: u8) -> bool {
    kind == CMD_PLACE || kind == CMD_MODIFY || kind == CMD_LEVERAGE
}

/// Whether class `class`'s batch grows ([`grows`]); a control batch by
/// its one command's kind. A read never grows: it goes while halted (the
/// reconciliation and the sweep after a halt need it).
#[inline(always)]
const fn growing(class: usize, control_kind: u8) -> bool {
    class != CLASS_CANCEL && class != CLASS_READ && (class != CLASS_CONTROL || grows(control_kind))
}

/// A command's kind in an event's two `flags` bits: place, modify or
/// cancel; 0 for a control command (the arm knows which by its client id).
#[inline(always)]
const fn cmd_bits(kind: u8) -> u8 {
    if kind <= CMD_CANCEL {
        kind
    } else {
        0
    }
}

/// Whether a reply status is one an item of `class` can have.
#[inline(always)]
const fn fits(class: u8, status: u8) -> bool {
    if status == ITEM_ERROR {
        return true;
    }
    if class as usize == CLASS_CANCEL {
        status == ITEM_SUCCESS
    } else {
        status == ITEM_RESTING || status == ITEM_FILLED
    }
}

/// A read's end event: the read command's correlation, `rows` in `aux`.
#[inline(always)]
fn read_end(item: &ItemRef, kind: u8, read: u8, rows: u64, now: u64) -> ExecEvent {
    ExecEvent {
        ts_ns: now,
        client_oid: item.client_oid,
        aux: rows,
        kind,
        reason: read,
        slot: item.slot,
        ..ExecEvent::default()
    }
}

/// Our slot and client id from a row's cloid, or foreign.
#[inline(always)]
fn owner(cloid: Option<[u8; 16]>) -> (u8, u64) {
    match cloid.map(|c| decode(&c)) {
        Some(Owner::Ours {
            strategy_id,
            client_oid,
        }) => (strategy_id, client_oid),
        _ => (SLOT_FOREIGN, 0),
    }
}

/// The dedupe key of an `orderUpdates` row: never 0.
#[inline(always)]
fn update_key(u: &OrderUpdate) -> u64 {
    let k = u.oid.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ u.status_ms.rotate_left(17)
        ^ (u.sz_1e8 as u64).rotate_left(31)
        ^ (u64::from(u.kind) << 56)
        ^ (u64::from(u.reason) << 48);
    k.max(1)
}

/// Emit, giving the engine a bounded moment to free a slot first; a
/// drop after that is counted and halts the arm, inside `emit` (XG-2).
#[inline]
fn emit_to(evt: &mut EvtTx<ExecEvent, EVT_RING>, ev: &ExecEvent) {
    if evt.room() == 0 {
        let t0 = now_ns();
        while evt.room() == 0 && now_ns().saturating_sub(t0) < ROOM_WAIT_NS {
            core::hint::spin_loop();
        }
    }
    let _ = evt.emit(ev);
}

/// A stream event about the gateway itself (no order).
#[inline]
fn gateway_event(kind: u8, aux: u64, venue_ms: u64, now_ns: u64) -> ExecEvent {
    ExecEvent {
        ts_ns: now_ns,
        venue_ms,
        aux,
        asset: ASSET_UNKNOWN,
        kind,
        slot: SLOT_FOREIGN,
        ..ExecEvent::default()
    }
}

/// The event an answer's status makes of an item.
#[inline]
fn answer_event(r: &ItemRef, s: &ItemStatus, id: u64, now_ns: u64) -> ExecEvent {
    let mut ev = r.event(EV_ACK, ORDER_EVENT_REASON_NONE, id, now_ns);
    match s.kind {
        ITEM_RESTING => {
            ev.oid = s.oid;
            ev.flags |= EVF_RESTING;
        }
        ITEM_FILLED => {
            ev.oid = s.oid;
            ev.px_1e8 = s.avg_px_1e8;
            ev.sz_1e8 = s.total_sz_1e8;
            ev.flags |= EVF_FILLED;
        }
        ITEM_SUCCESS => {}
        _ => {
            ev.kind = EV_REJECTED;
            ev.reason = s.reason;
        }
    }
    ev
}

/// Everything a frame, an answer or a deadline touches — split from the
/// connections so a connection can hand its frames here while it is
/// being read.
struct Router {
    evt: EvtTx<ExecEvent, EVT_RING>,
    stats: Arc<GatewayStats>,
    pending: Pending<PostRec, PENDING_SLOTS>,
    timers: TimerQueue<TIMER_SLOTS>,
    items: [ItemStatus; MAX_ORDERS],
    fills: Box<[UserFill]>,
    updates: Box<[OrderUpdate]>,
    tids: Box<TidRing<SNAPSHOT_RING>>,
    seen_updates: Box<TidRing<UPDATE_RING>>,
    snapshot_absorbed: bool,
    coins: CoinMap,
    counters: GatewayCounters,
    /// Items of sent posts and open batches: each owes one answer event.
    inflight_items: usize,
    /// Posts answered per connection since the last settle.
    answered: [u32; 2],
    /// Data moved (a matched answer or a pong) since the last settle.
    healthy: bool,
    /// Read answers are scanned into these (XH4d).
    open_rows: Box<[OpenOrder; MAX_OPEN_ORDERS]>,
    positions: Box<[PerpPosition; MAX_POSITIONS]>,
    /// The sweep read's orders of ours, and its outcome (`SWEEP_*`).
    sweep_hits: Box<[SweepHit; MAX_OPEN_ORDERS]>,
    sweep_n: usize,
    sweep_outcome: u8,
}

impl Router {
    /// Boot: every table allocated here.
    fn new(evt: EvtTx<ExecEvent, EVT_RING>, stats: Arc<GatewayStats>, coins: CoinMap) -> Self {
        Self {
            evt,
            stats,
            pending: Pending::new(BLANK_POST),
            timers: TimerQueue::new(),
            items: [ItemStatus::default(); MAX_ORDERS],
            fills: vec![UserFill::default(); SNAPSHOT_RING].into_boxed_slice(),
            updates: vec![OrderUpdate::default(); MAX_UPDATES].into_boxed_slice(),
            // COPY: two 64 KiB dedupe rings built on the stack and moved
            // to the heap, once at boot — rejected: an unsafe in-place
            // initialisation of a type whose fields are private.
            tids: Box::new(TidRing::new()),
            seen_updates: Box::new(TidRing::new()),
            snapshot_absorbed: false,
            coins,
            counters: GatewayCounters::default(),
            inflight_items: 0,
            answered: [0; 2],
            healthy: false,
            open_rows: boxed_array(OpenOrder::default()),
            positions: boxed_array(PerpPosition::default()),
            sweep_hits: boxed_array(SweepHit::default()),
            sweep_n: 0,
            sweep_outcome: SWEEP_NONE,
        }
    }

    fn on_frame(&mut self, frame: &[u8], now: u64) {
        match classify(frame) {
            Ok(WsIn::Post { kind, id, payload }) => match kind {
                PostKind::Action => self.on_answer(id, payload.of(frame), true, false, now),
                PostKind::Error => self.on_answer(id, payload.of(frame), false, false, now),
                PostKind::Info => self.on_read_answer(id, payload.of(frame), now),
            },
            Ok(WsIn::UserFills) => self.on_user_fills(frame, now),
            Ok(WsIn::OrderUpdates) => self.on_order_updates(frame, now),
            Ok(WsIn::Pong) => self.healthy = true,
            Ok(WsIn::Error(_)) => self.counters.ws_errors += 1,
            Ok(WsIn::SubAck | WsIn::Other) => {}
            // An answer that names no post: its post times out (E-5).
            Err(_) => self.counters.answers_unreadable += 1,
        }
    }

    /// The answer to post `id`: an `/exchange` reply (`action`), or the
    /// venue API layer's error message. `resend`: the post is a REST
    /// re-send of a WebSocket original that may have landed — only a
    /// per-item answer settles it.
    fn on_answer(&mut self, id: u64, body: &[u8], action: bool, resend: bool, now: u64) {
        let Some(class) = self.pending.get(id).map(|r| usize::from(r.class)) else {
            self.stats.note_reply_unmatched();
            return;
        };
        self.stats.note_reply();
        if !action {
            // Not the exchange's answer about the action: in doubt.
            self.fail_all(id, EV_UNANSWERED, reason_of_error(body), now);
            return;
        }
        if class == CLASS_READ {
            // A read answered as an action: in doubt (the sweep's own
            // read fails its round).
            self.counters.answers_mismatched += 1;
            self.fail_all(id, EV_UNANSWERED, REASON_MALFORMED, now);
            return;
        }
        if class == CLASS_CONTROL {
            self.on_control_answer(id, body, resend, now);
            return;
        }
        match scan_items(body, &mut self.items) {
            Ok(ItemsReply::Items(m)) => {
                let Some(rec) = self.pending.get(id) else {
                    return;
                };
                let mut sound = m == usize::from(rec.n);
                let mut i = 0usize;
                while sound && i < m {
                    sound = fits(rec.class, self.items[i].kind);
                    i += 1;
                }
                if !sound {
                    self.counters.answers_mismatched += 1;
                    self.fail_all(id, EV_UNANSWERED, REASON_MALFORMED, now);
                    return;
                }
                let mut i = 0usize;
                while i < m {
                    let ev = answer_event(&rec.items[i], &self.items[i], id, now);
                    emit_to(&mut self.evt, &ev);
                    i += 1;
                }
                self.healthy = true;
                self.finish(id, now);
            }
            Ok(ItemsReply::Refused { reason, .. }) if resend => {
                self.fail_all(id, EV_UNANSWERED, reason, now);
            }
            Ok(ItemsReply::Refused { reason, .. }) => self.fail_all(id, EV_REJECTED, reason, now),
            Err(_) => self.fail_all(id, EV_UNANSWERED, REASON_MALFORMED, now),
        }
    }

    /// A control action's answer. Accepted is `{"type":"default"}` and
    /// nothing else; `"status":"err"` is the venue's refusal (in doubt on
    /// a re-send, E-5); an item-shaped answer does not fit a post that has
    /// no items, and it and anything unreadable are in doubt.
    fn on_control_answer(&mut self, id: u64, body: &[u8], resend: bool, now: u64) {
        match scan(body) {
            Ok(HlResponse::Ok(ok)) if ok.is_default() => {
                let Some(rec) = self.pending.get(id) else {
                    return;
                };
                let ev = rec.items[0].event(EV_ACK, ORDER_EVENT_REASON_NONE, id, now);
                emit_to(&mut self.evt, &ev);
                self.healthy = true;
                self.finish(id, now);
            }
            Ok(HlResponse::Err { msg }) => {
                let kind = if resend { EV_UNANSWERED } else { EV_REJECTED };
                self.fail_all(id, kind, reason_of_error(msg.of(body)), now);
            }
            Ok(HlResponse::Ok(_)) => {
                self.counters.answers_mismatched += 1;
                self.fail_all(id, EV_UNANSWERED, REASON_MALFORMED, now);
            }
            Err(_) => self.fail_all(id, EV_UNANSWERED, REASON_MALFORMED, now),
        }
    }

    /// A read's answer (XH4d): its rows as events, then `EV_READ_DONE` —
    /// or `EV_READ_FAILED` when it does not scan. The gateway's own sweep
    /// read keeps its rows (ours only) and emits nothing.
    fn on_read_answer(&mut self, id: u64, body: &[u8], now: u64) {
        let Some(rec) = self.pending.get(id) else {
            self.stats.note_reply_unmatched();
            return;
        };
        self.stats.note_reply();
        if usize::from(rec.class) != CLASS_READ {
            // A read's answer to a post that is no read: in doubt.
            self.counters.answers_mismatched += 1;
            self.fail_all(id, EV_UNANSWERED, REASON_MALFORMED, now);
            return;
        }
        let item = rec.items[0];
        let ok = if item.kind == CMD_READ_ORDERS {
            self.read_orders(&item, body, now)
        } else {
            self.read_state(&item, body, now)
        };
        if ok {
            self.counters.reads += 1;
            self.healthy = true;
        } else {
            self.counters.reads_failed += 1;
            if item.slot == SWEEP_SLOT {
                self.sweep_outcome = SWEEP_FAILED;
            } else {
                let read = if item.kind == CMD_READ_ORDERS {
                    READ_ORDERS
                } else {
                    READ_STATE
                };
                emit_to(
                    &mut self.evt,
                    &read_end(&item, EV_READ_FAILED, read, 0, now),
                );
            }
        }
        self.finish(id, now);
    }

    /// `{"type":"frontendOpenOrders","data":[…]}` → `EV_OPEN_ORDER` rows
    /// (`aux` = the read command's client id) and `EV_READ_DONE`, or the
    /// sweep's hits. `false`: it does not scan.
    fn read_orders(&mut self, item: &ItemRef, body: &[u8], now: u64) -> bool {
        let Some(a) = array_start(body, b"\"data\"") else {
            return false;
        };
        let data = &body[a - 1..];
        let Ok(n) = scan_open_orders(data, &mut self.open_rows[..]) else {
            return false;
        };
        let n = n.min(self.open_rows.len());
        if item.slot == SWEEP_SLOT {
            let mut hits = 0usize;
            let mut i = 0usize;
            while i < n {
                let r = &self.open_rows[i];
                let (slot, client_oid) = owner(r.cloid);
                if slot != SLOT_FOREIGN {
                    // `hits` ≤ `i` < `n` ≤ the table: the mask folds the
                    // check.
                    self.sweep_hits[hits & (MAX_OPEN_ORDERS - 1)] = SweepHit {
                        client_oid,
                        asset: self.coins.asset(r.coin.of(data)),
                        slot,
                    };
                    hits += 1;
                }
                i += 1;
            }
            self.sweep_n = hits;
            self.sweep_outcome = SWEEP_READ;
            return true;
        }
        let mut i = 0usize;
        while i < n {
            let r = &self.open_rows[i];
            let (slot, client_oid) = owner(r.cloid);
            let ev = ExecEvent {
                ts_ns: now,
                client_oid,
                oid: r.oid,
                aux: item.client_oid,
                asset: self.coins.asset(r.coin.of(data)),
                kind: EV_OPEN_ORDER,
                slot,
                ..ExecEvent::default()
            };
            emit_to(&mut self.evt, &ev);
            i += 1;
        }
        emit_to(
            &mut self.evt,
            &read_end(item, EV_READ_DONE, READ_ORDERS, n as u64, now),
        );
        true
    }

    /// `{"type":"clearinghouseState","data":{…}}` → `EV_POSITION` rows and
    /// `EV_READ_DONE` (the account value, the sheet's time). `false`: it
    /// does not scan.
    fn read_state(&mut self, item: &ItemRef, body: &[u8], now: u64) -> bool {
        let Some((s, e)) = object_field(body, b"\"data\"") else {
            return false;
        };
        let data = &body[s..e];
        let Ok((value, n)) = scan_perp_state(data, &mut self.positions[..]) else {
            return false;
        };
        let n = n.min(self.positions.len());
        let mut i = 0usize;
        while i < n {
            let p = &self.positions[i];
            let ev = ExecEvent {
                ts_ns: now,
                sz_1e8: p.szi_1e8,
                aux: p.value_1e8.unsigned_abs(),
                asset: self.coins.asset(p.coin.of(data)),
                kind: EV_POSITION,
                slot: item.slot,
                client_oid: item.client_oid,
                ..ExecEvent::default()
            };
            emit_to(&mut self.evt, &ev);
            i += 1;
        }
        let mut end = read_end(item, EV_READ_DONE, READ_STATE, n as u64, now);
        end.px_1e8 = value;
        end.venue_ms = u64_field(data, b"\"time\"").unwrap_or(0);
        emit_to(&mut self.evt, &end);
        true
    }

    /// A REST answer: HTTP `status` and its body.
    fn on_rest_answer(&mut self, id: u64, status: u16, body: &[u8], resend: bool, now: u64) {
        if (200..300).contains(&status) {
            self.on_answer(id, body, true, resend, now);
            return;
        }
        self.stats.note_reply();
        if status == 429 && !resend {
            // The venue's own "not processed, slow down".
            self.fail_all(id, EV_REJECTED, REASON_RATE_LIMIT, now);
        } else {
            let reason = if status == 429 {
                REASON_RATE_LIMIT
            } else {
                REASON_TRANSPORT
            };
            self.fail_all(id, EV_UNANSWERED, reason, now);
        }
    }

    /// One event of `kind` per item of post `id`, then release it.
    fn fail_all(&mut self, id: u64, kind: u8, reason: u8, now: u64) {
        let Some(rec) = self.pending.get(id) else {
            return;
        };
        if rec.items[0].slot == SWEEP_SLOT {
            // The gateway's own sweep read: nothing to tell the arm.
            self.sweep_outcome = SWEEP_FAILED;
            self.finish(id, now);
            return;
        }
        let mut i = 0usize;
        while i < usize::from(rec.n) {
            emit_to(&mut self.evt, &rec.items[i].event(kind, reason, id, now));
            i += 1;
        }
        self.finish(id, now);
    }

    /// Release post `id`: its deadline, its connection's count, its
    /// latency sample.
    fn finish(&mut self, id: u64, now: u64) {
        let Some(rec) = self.pending.get(id) else {
            return;
        };
        let (timer, conn, n, class, written) =
            (rec.timer, rec.conn, rec.n, rec.class, rec.written_ns);
        if let Some(t) = timer {
            let _ = self.timers.cancel(t);
        }
        if conn < 2 {
            self.answered[usize::from(conn)] += 1;
        }
        self.inflight_items -= usize::from(n);
        if written != 0 && usize::from(class) != CLASS_READ {
            let rtt = now.saturating_sub(written);
            if usize::from(class) == CLASS_CANCEL {
                self.stats.cancel_rtt.record(rtt);
            } else {
                self.stats.post_rtt.record(rtt);
            }
        }
        let _ = self.pending.release(id);
    }

    fn on_user_fills(&mut self, frame: &[u8], now: u64) {
        let (n, is_snapshot) = match scan_user_fills(frame, &mut self.fills) {
            Ok(v) => v,
            Err(_) => {
                self.counters.fills_scan_failed += 1;
                emit_to(
                    &mut self.evt,
                    &gateway_event(EV_GAP, GAP_FILLS_UNREADABLE, 0, now),
                );
                return;
            }
        };
        if is_snapshot && !self.snapshot_absorbed {
            // The process's first snapshot is HISTORY: the positions it
            // made are the boot reconciliation's, not new trades.
            self.snapshot_absorbed = true;
            let mut latest = 0u64;
            let mut i = 0usize;
            while i < n {
                let _ = self.tids.admit(self.fills[i].tid);
                latest = latest.max(self.fills[i].time_ms);
                i += 1;
            }
            self.counters.snapshot_rows += n as u64;
            emit_to(
                &mut self.evt,
                &gateway_event(EV_SNAPSHOT, n as u64, latest, now),
            );
            return;
        }
        let wall_ns = now_ms().saturating_mul(1_000_000);
        let mut i = 0usize;
        while i < n {
            let f = &self.fills[i];
            if self.tids.admit(f.tid) {
                let (slot, client_oid) = owner(f.cloid);
                let mut flags = if f.is_buy { EVF_BUY } else { 0 };
                if f.crossed {
                    flags |= EVF_CROSSED;
                }
                if is_snapshot {
                    flags |= EVF_SNAPSHOT;
                }
                let fill = ExecEvent {
                    ts_ns: now,
                    venue_ms: f.time_ms,
                    client_oid,
                    oid: f.oid,
                    px_1e8: f.px_1e8,
                    sz_1e8: f.sz_1e8,
                    aux: f.tid,
                    asset: self.coins.asset(f.coin.of(frame)),
                    kind: EV_FILL,
                    reason: if f.is_settlement {
                        REASON_SETTLEMENT
                    } else {
                        0
                    },
                    slot,
                    flags,
                };
                let meta = ExecEvent {
                    kind: EV_FILL_META,
                    px_1e8: f.fee_1e8,
                    reason: 0,
                    ..fill
                };
                if !is_snapshot {
                    self.stats
                        .fill_venue_to_local
                        .record(wall_ns.saturating_sub(f.time_ms.saturating_mul(1_000_000)));
                }
                emit_to(&mut self.evt, &fill);
                emit_to(&mut self.evt, &meta);
                self.counters.fills += 1;
            }
            i += 1;
        }
    }

    fn on_order_updates(&mut self, frame: &[u8], now: u64) {
        let n = match scan_order_updates(frame, &mut self.updates) {
            Ok(n) => n,
            Err(_) => {
                self.counters.updates_scan_failed += 1;
                emit_to(
                    &mut self.evt,
                    &gateway_event(EV_GAP, GAP_UPDATES_UNREADABLE, 0, now),
                );
                return;
            }
        };
        let mut i = 0usize;
        while i < n {
            let u = &self.updates[i];
            if u.kind == 0 {
                self.counters.updates_ignored += 1;
            } else if self.seen_updates.admit(update_key(u)) {
                let (slot, client_oid) = owner(u.cloid);
                let ev = ExecEvent {
                    ts_ns: now,
                    venue_ms: u.status_ms,
                    client_oid,
                    oid: u.oid,
                    px_1e8: u.px_1e8,
                    sz_1e8: u.sz_1e8,
                    aux: 0,
                    asset: self.coins.asset(u.coin.of(frame)),
                    kind: u.kind,
                    reason: u.reason,
                    slot,
                    flags: if u.is_buy { EVF_BUY } else { 0 },
                };
                emit_to(&mut self.evt, &ev);
                self.counters.updates += 1;
            }
            i += 1;
        }
    }
}

/// The gateway. Built at boot on the engine thread, then moved to its
/// own thread and [`run`](Self::run) there.
pub struct HlGateway {
    rt: Router,
    cmd: CmdRx<HlCmd, CMD_RING>,
    conns: [GwConn; 2],
    mbb: Mbb,
    http: HlHttp,
    poll: Poll,
    events: Events,
    host: String,
    port: u16,
    addr: SocketAddr,
    server_name: ServerName<'static>,
    tls: Arc<ClientConfig>,
    master_hex: String,
    slots: u8,
    sk: secp256k1::SecretKey,
    network: Network,
    nonce: Nonce,
    batches: Box<Batches>,
    mp: Box<[u8]>,
    tx: Box<[u8]>,
    next_id: u64,
    seq: u64,
    post_timeout_ns: u64,
    idle: Idle,
    worked: bool,
    lives: u64,
    dial_fails: u32,
    /// The pending replacement is for a socket that went silent: its
    /// `orderUpdates` since the silence began are lost.
    silent_replace: bool,
    /// The account, for the reads' bodies (XH4d).
    master: [u8; 20],
    /// The sweep request being worked, its phase, its round, and when a
    /// settling round re-reads (XH4d).
    sweep_seq: u64,
    sweep_phase: u8,
    sweep_round: u8,
    sweep_until: u64,
    /// The last heartbeat stamp.
    beat_ns: u64,
}

impl HlGateway {
    /// Boot: resolve, allocate every buffer, hold the signing key. No
    /// socket opens until the first turn.
    ///
    /// # Errors
    /// [`GatewayErr`].
    pub fn new(
        cfg: GatewayCfg,
        end: HlGatewayEnd,
        sk: secp256k1::SecretKey,
        coins: CoinMap,
    ) -> Result<Self, GatewayErr> {
        let addr = resolve(&cfg.host, cfg.port)?;
        let server_name = TlsTransport::server_name_from_host(&cfg.host)
            .map_err(|_| GatewayErr::BadServerName)?;
        let http =
            HlHttp::new(&cfg.host, cfg.port, Arc::clone(&cfg.tls)).map_err(|_| GatewayErr::Http)?;
        let poll = Poll::new().map_err(|_| GatewayErr::Poll)?;
        let GatewayEnd { cmd, evt, stats } = end;
        // COPY: the ≤ 1.4 KiB coin table moves into place once at boot,
        // and the gateway itself (a few KiB, its tables on the heap)
        // moves to its thread once — rejected: boxing each, which buys
        // nothing at boot.
        Ok(Self {
            rt: Router::new(evt, stats, coins),
            cmd,
            conns: [GwConn::new(0, KEEPALIVE), GwConn::new(1, KEEPALIVE)],
            mbb: Mbb::new(
                Backoff::default_for_ingress(now_ns()),
                HEALTHY_NS,
                DIAL_BUDGET_NS,
                DRAIN_BUDGET_NS,
            ),
            http,
            poll,
            events: Events::with_capacity(16),
            master_hex: crate::config::hex20(&cfg.master),
            master: cfg.master,
            sweep_seq: 0,
            sweep_phase: SW_IDLE,
            sweep_round: 0,
            sweep_until: 0,
            beat_ns: 0,
            host: cfg.host,
            port: cfg.port,
            addr,
            server_name,
            tls: cfg.tls,
            slots: cfg.slots,
            sk,
            network: cfg.network,
            nonce: Nonce::new(),
            // COPY: the ≈ 7.7 KiB batch arrays, built on the stack and
            // moved to the heap once at boot — rejected: an unsafe
            // in-place initialisation (as the dedupe rings).
            batches: Box::new(Batches::new()),
            mp: vec![0u8; MAX_ACTION].into_boxed_slice(),
            tx: vec![0u8; TX_CAP].into_boxed_slice(),
            next_id: 1,
            seq: 0,
            post_timeout_ns: cfg.post_timeout_ns,
            idle: cfg.idle,
            worked: false,
            lives: 0,
            dial_fails: 0,
            silent_replace: false,
        })
    }

    /// Turn until `stop` is set. The owner spawns the thread (XH4f pins
    /// it, drains it and orders the shutdown); a panic here ends the
    /// process (`panic = "abort"`: a gateway crash is fail-fast, and the
    /// boot sweep cancels on relaunch).
    pub fn run(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            self.turn();
        }
    }

    /// One turn — see the module docs.
    pub fn turn(&mut self) {
        let wait = self
            .idle
            .timeout_ns(self.worked, now_ns(), self.rt.timers.next_deadline());
        let failed = self.poll_io(wait);
        let now = now_ns();
        let mut worked = false;
        let mut s = 0u8;
        while s < 2 {
            if failed[usize::from(s)] {
                self.conn_down(s, now);
                worked = true;
            }
            s += 1;
        }
        worked |= self.read_conns(now);
        worked |= self.drive_mbb(now);
        worked |= self.pump_commands(now);
        worked |= self.sweep(now);
        worked |= self.expire(now);
        self.keepalive(now);
        self.settle(now);
        self.rt
            .stats
            .set_gauges(self.rt.pending.live() as u64, self.mbb.active().is_some());
        if now.saturating_sub(self.beat_ns) >= BEAT_NS {
            self.rt.stats.beat(now);
            self.beat_ns = now;
        }
        self.worked = worked;
    }

    /// Ask for the active connection to be replaced make-before-break.
    pub fn request_replace(&mut self) {
        self.mbb.request_replace();
    }

    /// Whether a connection is live and carrying posts.
    #[must_use]
    pub fn ws_live(&self) -> bool {
        self.mbb.active().is_some()
    }

    /// Completed make-before-break swaps.
    #[must_use]
    pub fn swaps(&self) -> u64 {
        self.mbb.swaps()
    }

    /// The gateway's own counters.
    #[must_use]
    pub fn counters(&self) -> &GatewayCounters {
        &self.rt.counters
    }

    // ---- I/O ----------------------------------------------------------

    fn poll_io(&mut self, wait_ns: u64) -> [bool; 2] {
        let mut failed = [false; 2];
        if self
            .poll
            .poll(&mut self.events, Some(Duration::from_nanos(wait_ns)))
            .is_err()
        {
            return failed;
        }
        for ev in self.events.iter() {
            let s = ev.token().0;
            if s < 2 && self.conns[s].on_event(ev).is_err() {
                failed[s] = true;
            }
        }
        failed
    }

    fn read_conns(&mut self, now: u64) -> bool {
        let mut worked = false;
        let mut s = 0u8;
        while s < 2 {
            let i = usize::from(s);
            if self.conns[i].phase() != Phase::Down {
                let rt = &mut self.rt;
                let res = self.conns[i].service(
                    now,
                    self.host.as_bytes(),
                    self.master_hex.as_bytes(),
                    self.poll.registry(),
                    &mut |frame: &[u8]| rt.on_frame(frame, now),
                );
                match res {
                    Ok(moved) => {
                        worked |= moved;
                        if self.conns[i].phase() == Phase::Ready
                            && self.mbb.state(s) == SlotState::Dialing
                        {
                            self.went_live(s, now);
                        }
                    }
                    Err(_) => {
                        self.conn_down(s, now);
                        worked = true;
                    }
                }
            }
            s += 1;
        }
        worked
    }

    /// Slot `s` is subscribed and ready: it takes the new posts. Up after
    /// a gap with no live socket, or replacing one that went silent, the
    /// `orderUpdates` pushed meanwhile are lost (that channel has no
    /// snapshot): the arm re-checks.
    fn went_live(&mut self, s: u8, now: u64) {
        let gap = (self.mbb.active().is_none() && self.lives > 0) || self.silent_replace;
        self.silent_replace = false;
        self.mbb.on_live(s, now);
        self.dial_fails = 0;
        self.lives += 1;
        if self.lives > 1 {
            self.rt.stats.note_reconnect();
        }
        if gap {
            emit_to(
                &mut self.rt.evt,
                &gateway_event(EV_GAP, GAP_RECONNECT, 0, now),
            );
        }
    }

    fn drive_mbb(&mut self, now: u64) -> bool {
        match self.mbb.poll(now) {
            MbbAction::Idle => false,
            MbbAction::Dial(s) => {
                self.rt.counters.dials += 1;
                if self.dial_fails >= RERESOLVE_AFTER && self.mbb.active().is_none() {
                    // Cold: getaddrinfo on this thread — only while no
                    // socket carries posts, after RERESOLVE_AFTER failed
                    // dials.
                    if let Ok(a) = resolve(&self.host, self.port) {
                        self.addr = a;
                    }
                    self.dial_fails = 0;
                }
                let dialed = self.conns[usize::from(s)].dial(
                    self.addr,
                    &self.server_name,
                    &self.tls,
                    self.poll.registry(),
                    now,
                );
                if dialed.is_err() {
                    self.dial_fails += 1;
                    self.mbb.on_down(s, now);
                }
                true
            }
            MbbAction::Retire(s) => {
                self.conn_down(s, now);
                true
            }
        }
    }

    /// Strand every post in flight on connection `s` (E-5: in doubt),
    /// close it, and tell the machine.
    fn conn_down(&mut self, s: u8, now: u64) {
        let mut slot = 0usize;
        while slot < PENDING_SLOTS {
            if let Some(id) = self.rt.pending.id_at(slot) {
                if self.rt.pending.get(id).is_some_and(|r| r.conn == s) {
                    self.rt.fail_all(id, EV_UNANSWERED, REASON_TRANSPORT, now);
                }
            }
            slot += 1;
        }
        let i = usize::from(s);
        if self.conns[i].phase() != Phase::Ready {
            self.dial_fails += 1;
        }
        self.conns[i].close();
        self.rt.answered[i] = 0;
        self.mbb.on_down(s, now);
        self.rt.counters.conn_drops += 1;
    }

    fn keepalive(&mut self, now: u64) {
        let mut s = 0u8;
        while s < 2 {
            let i = usize::from(s);
            if self.conns[i].phase() == Phase::Ready {
                match self.conns[i].maintain(now) {
                    Ok(false) => {
                        if now.saturating_sub(self.conns[i].last_rx_ns()) >= REPLACE_QUIET_NS
                            && self.mbb.active() == Some(s)
                        {
                            self.mbb.request_replace();
                            self.silent_replace = true;
                        }
                    }
                    Ok(true) | Err(_) => self.conn_down(s, now),
                }
            }
            s += 1;
        }
    }

    /// Apply the turn's answers to their connections; reset the backoff
    /// on a session that moved data; close a drained connection.
    fn settle(&mut self, now: u64) {
        let mut s = 0u8;
        while s < 2 {
            let i = usize::from(s);
            self.conns[i].note_answered(self.rt.answered[i]);
            self.rt.answered[i] = 0;
            if self.mbb.state(s) == SlotState::Draining && self.conns[i].posts() == 0 {
                self.mbb.on_drained(s, now);
                self.conns[i].close();
            }
            s += 1;
        }
        if self.rt.healthy {
            self.mbb.note_healthy(now);
            self.rt.healthy = false;
        }
    }

    // ---- commands -----------------------------------------------------

    /// XG-2: no room for one more command — every class's open batch, the
    /// streams and a read's answer keep their room.
    fn xg2_full(&self) -> bool {
        let room_needed =
            self.rt.inflight_items + MAX_ORDERS * CLASSES + STREAM_RESERVE + READ_RESERVE;
        self.rt.pending.live() + CLASSES > MAX_IN_FLIGHT || self.rt.evt.room() < room_needed
    }

    fn pump_commands(&mut self, now: u64) -> bool {
        let mut worked = false;
        loop {
            if self.xg2_full() {
                break;
            }
            let c = match self.cmd.pop() {
                // COPY: the 64 B command out of its ring slot (one cache
                // line) — `admit` needs the whole gateway (it may post
                // every open batch), which it cannot borrow while the
                // slot's guard borrows the ring — rejected: splitting the
                // gateway around the ring for one line's copy.
                Some(p) => *p,
                None => break,
            };
            worked = true;
            self.admit(&c, now);
        }
        let mut k = 0usize;
        while k < CLASSES {
            if self.batches.head[k].n > 0 {
                self.flush_open(now);
                return true;
            }
            k += 1;
        }
        worked
    }

    /// Put one command into its class batch, or refuse it unsent.
    fn admit(&mut self, c: &HlCmd, now: u64) {
        let growth = grows(c.kind);
        let refusal = if c.slot >= 8 || self.slots & (1u8 << c.slot) == 0 {
            Some(REASON_LOCAL)
        } else if growth && self.rt.stats.halt_reason() != HALT_NONE {
            Some(REASON_HALTED)
        } else if growth && !self.rt.snapshot_absorbed {
            Some(REASON_NOT_READY)
        } else {
            None
        };
        if let Some(reason) = refusal {
            self.refuse(c, reason, now);
            return;
        }
        let Some(k) = class_of(c) else {
            self.refuse(c, REASON_LOCAL, now);
            return;
        };
        if k == CLASS_CONTROL || k == CLASS_READ {
            self.admit_alone(c, k, now);
            return;
        }
        if (c.kind == CMD_CANCEL || c.kind == CMD_MODIFY) && self.names_open_item(c) {
            self.flush_open(now);
        }
        if self.batches.head[k].n == MAX_ORDERS {
            // Every open batch, in the order they opened: a full batch
            // never overtakes an older one.
            self.flush_open(now);
        }
        if self.batches.head[k].n == 0 && !self.open(k, now) {
            self.refuse(c, REASON_LOCAL, now);
            return;
        }
        let b = &mut *self.batches;
        let h = &mut b.head[k];
        let i = h.n;
        match k {
            CLASS_ALO => b.alo[i] = c.order_wire(),
            CLASS_TAKER => b.taker[i] = c.order_wire(),
            CLASS_MODIFY => c.modify_into(&mut b.modify[i]),
            _ => b.cancel[i] = c.cancel_wire(),
        }
        if c.expires_after_ms != 0 && (h.min_exp == 0 || c.expires_after_ms < h.min_exp) {
            h.min_exp = c.expires_after_ms;
        }
        h.n += 1;
        if let Some(rec) = self.rt.pending.get_mut(h.id) {
            rec.items[i] = ItemRef::of(c);
            rec.n = h.n as u8;
        }
        self.rt.inflight_items += 1;
    }

    /// A control command or a read posts alone, after every open batch:
    /// nothing admitted before it goes after it, and nothing after it
    /// before. A control post carries the command's own `expiresAfter`,
    /// if any — the dead man's posts do, so a late one cannot land after
    /// a later one (E-5); a read is unsigned and carries none.
    fn admit_alone(&mut self, c: &HlCmd, k: usize, now: u64) {
        self.flush_open(now);
        if !self.open(k, now) {
            self.refuse(c, REASON_LOCAL, now);
            return;
        }
        if k == CLASS_CONTROL {
            self.batches.control = ControlWire {
                kind: c.kind,
                is_cross: c.flags & CMD_FLAG_CROSS != 0,
                asset: c.asset,
                leverage: c.arg,
                time: if c.time_ms == 0 {
                    None
                } else {
                    Some(c.time_ms)
                },
            };
        }
        let h = &mut self.batches.head[k];
        h.n = 1;
        h.min_exp = if k == CLASS_CONTROL {
            c.expires_after_ms
        } else {
            0
        };
        if let Some(rec) = self.rt.pending.get_mut(h.id) {
            rec.items[0] = ItemRef::of(c);
            rec.n = 1;
        }
        self.rt.inflight_items += 1;
    }

    /// Refuse a command unsent.
    fn refuse(&mut self, c: &HlCmd, reason: u8, now: u64) {
        self.rt.counters.local_refusals += 1;
        if c.slot == SWEEP_SLOT {
            // The sweep's own read: its round fails; the arm hears nothing.
            self.rt.sweep_outcome = SWEEP_FAILED;
            return;
        }
        let ev = ItemRef::of(c).event(EV_REJECTED, reason, 0, now);
        emit_to(&mut self.rt.evt, &ev);
    }

    /// Whether a cancel's or modify's target is still in an open batch.
    fn names_open_item(&self, c: &HlCmd) -> bool {
        let target = if c.kind == CMD_CANCEL {
            c.client_oid
        } else {
            c.prev_client_oid
        };
        let mut k = 0usize;
        while k < CLASSES {
            let h = &self.batches.head[k];
            if h.n > 0 {
                if let Some(rec) = self.rt.pending.get(h.id) {
                    let mut i = 0usize;
                    while i < h.n {
                        let r = &rec.items[i];
                        if r.slot == c.slot && (r.client_oid == target || r.prev == target) {
                            return true;
                        }
                        i += 1;
                    }
                }
            }
            k += 1;
        }
        false
    }

    /// Open class `k`'s batch: claim its post id and pending slot.
    fn open(&mut self, k: usize, now: u64) -> bool {
        let mut id = self.next_id;
        let mut tries = 0usize;
        while !self.rt.pending.is_free(id) && tries < PENDING_SLOTS {
            id = id.wrapping_add(1).max(1);
            tries += 1;
        }
        let Ok(rec) = self.rt.pending.claim(id, now) else {
            return false;
        };
        rec.written_ns = 0;
        rec.timer = None;
        rec.n = 0;
        rec.class = k as u8;
        rec.conn = CONN_OPEN;
        self.next_id = id.wrapping_add(1).max(1);
        self.seq += 1;
        self.batches.head[k] = BatchHead {
            id,
            seq: self.seq,
            n: 0,
            min_exp: 0,
        };
        true
    }

    /// Post every open batch, in the order they opened.
    fn flush_open(&mut self, now: u64) {
        loop {
            let mut best = CLASSES;
            let mut k = 0usize;
            while k < CLASSES {
                let h = &self.batches.head[k];
                if h.n > 0 && (best == CLASSES || h.seq < self.batches.head[best].seq) {
                    best = k;
                }
                k += 1;
            }
            if best == CLASSES {
                return;
            }
            self.flush(best, now);
        }
    }

    /// Seal and send class `k`'s batch — unless a halt was raised since
    /// its items were admitted (growth only; cancels still go).
    fn flush(&mut self, k: usize, now: u64) {
        let h = self.batches.head[k];
        if h.n == 0 {
            return;
        }
        self.batches.head[k].n = 0;
        if growing(k, self.batches.control.kind) && self.rt.stats.halt_reason() != HALT_NONE {
            self.rt.fail_all(h.id, EV_REJECTED, REASON_HALTED, now);
            return;
        }
        match self.seal(k, &h) {
            Some(sealed) => self.send(k, h.id, sealed, now),
            None => {
                self.rt.counters.local_refusals += 1;
                self.rt.fail_all(h.id, EV_REJECTED, REASON_LOCAL, now);
            }
        }
    }

    /// Encode, sign and render the post into the TX buffer. `None` = the
    /// clock, the signer or a buffer refused: nothing was sent.
    fn seal(&mut self, k: usize, h: &BatchHead) -> Option<Sealed> {
        if k == CLASS_READ {
            return self.seal_read(h);
        }
        let n = h.n;
        let b = &*self.batches;
        let mp_n = match k {
            CLASS_ALO => encode_order(&mut self.mp, &b.alo[..n], GROUPING),
            CLASS_TAKER => encode_order(&mut self.mp, &b.taker[..n], GROUPING),
            CLASS_MODIFY => encode_batch_modify(&mut self.mp, &b.modify[..n]),
            CLASS_CANCEL => encode_cancel_by_cloid(&mut self.mp, &b.cancel[..n]),
            _ => encode_control(&b.control, &mut self.mp, false),
        }
        .ok()?;
        let ms = now_ms();
        if ms == 0 {
            return None;
        }
        let nonce = self.nonce.next(ms);
        let expires = if h.min_exp == 0 {
            None
        } else {
            Some(ms + u64::from(h.min_exp))
        };
        let sig = sign_action(
            &self.sk,
            &self.mp[..mp_n],
            nonce,
            Vault::None,
            expires,
            self.network,
        )
        .ok()?;
        let tx = &mut self.tx[ROOM..];
        let pfx = ws_post_open(tx, h.id).ok()?;
        let head = envelope_open(&mut tx[pfx..]).ok()?;
        let at = pfx + head;
        let aj = match k {
            CLASS_ALO => order_json(&mut tx[at..], &b.alo[..n], GROUPING),
            CLASS_TAKER => order_json(&mut tx[at..], &b.taker[..n], GROUPING),
            CLASS_MODIFY => batch_modify_json(&mut tx[at..], &b.modify[..n]),
            CLASS_CANCEL => cancel_by_cloid_json(&mut tx[at..], &b.cancel[..n]),
            _ => encode_control(&b.control, &mut tx[at..], true),
        }
        .ok()?;
        let env_len = envelope_close(&mut tx[pfx..], head + aj, nonce, &sig, None, expires).ok()?;
        let ws_len = ws_post_close(tx, pfx + env_len).ok()?;
        Some(Sealed {
            env: ROOM + pfx,
            env_len,
            ws_len,
        })
    }

    /// A read (XH4d): the post head, the `/info` body rendered in place
    /// after it, the tail — unsigned, no envelope, no REST twin.
    fn seal_read(&mut self, h: &BatchHead) -> Option<Sealed> {
        let kind = self.rt.pending.get(h.id)?.items[0].kind;
        let tx = &mut self.tx[ROOM..];
        let at = ws_info_open(tx, h.id).ok()?;
        let body = if kind == CMD_READ_ORDERS {
            open_orders_request(&mut tx[at..], &self.master)
        } else {
            perp_state_request(&mut tx[at..], &self.master)
        }
        .ok()?;
        let ws_len = ws_post_close(tx, at + body).ok()?;
        Some(Sealed {
            env: ROOM + at,
            env_len: 0,
            ws_len,
        })
    }

    /// Write a sealed post on the live connection, or by REST — a read
    /// only on the live connection: with none it is refused, unsent.
    fn send(&mut self, k: usize, id: u64, s: Sealed, now: u64) {
        let active = match self.mbb.active() {
            Some(a) if self.conns[usize::from(a)].phase() == Phase::Ready => a,
            _ if k == CLASS_READ => {
                self.rt.fail_all(id, EV_REJECTED, REASON_TRANSPORT, now);
                return;
            }
            _ => {
                self.post_rest(id, s, false);
                return;
            }
        };
        // XG-3: the deadline before the write; a growing post that
        // cannot time out is not sent, a cancel always is.
        let timer =
            match self
                .rt
                .timers
                .arm(now.saturating_add(self.post_timeout_ns), id, TIMER_POST)
            {
                Ok(t) => Some(t),
                Err(_) => {
                    self.halt(HALT_TIMER_FULL, now);
                    if growing(k, self.batches.control.kind) || k == CLASS_READ {
                        self.rt.counters.local_refusals += 1;
                        self.rt.fail_all(id, EV_REJECTED, REASON_LOCAL, now);
                        return;
                    }
                    self.rt.counters.cancels_untimed += 1;
                    None
                }
            };
        let a = usize::from(active);
        let mask = self.conns[a].next_mask();
        let Ok(start) = ws_frame_in_place(&mut self.tx, ROOM, WsOpcode::Text, s.ws_len, mask)
        else {
            if let Some(t) = timer {
                let _ = self.rt.timers.cancel(t);
            }
            self.rt.counters.local_refusals += 1;
            self.rt.fail_all(id, EV_REJECTED, REASON_LOCAL, now);
            return;
        };
        let written = now_ns();
        match self.conns[a].send(&self.tx[start..ROOM + s.ws_len]) {
            Ok(()) => {
                self.conns[a].note_post();
                if k != CLASS_READ {
                    self.rt.stats.note_action_sent();
                }
                if let Some(rec) = self.rt.pending.get_mut(id) {
                    rec.conn = active;
                    rec.timer = timer;
                    rec.written_ns = written;
                    // A read is no order: it feeds no order latency.
                    let n = if k == CLASS_READ {
                        0
                    } else {
                        usize::from(rec.n)
                    };
                    let mut i = 0usize;
                    while i < n {
                        self.rt
                            .stats
                            .push_to_write
                            .record(written.saturating_sub(rec.items[i].ts_ns));
                        i += 1;
                    }
                }
            }
            Err(_) => {
                // The socket died under the write: the post may have
                // left. Re-send the SAME signed bytes by REST — the mask
                // XORs back; never re-signed.
                if let Some(t) = timer {
                    let _ = self.rt.timers.cancel(t);
                }
                if k == CLASS_READ {
                    // A read that may have left answers on the dead
                    // socket only: in doubt, and the reader asks again.
                    self.conn_down(active, now);
                    self.rt.fail_all(id, EV_UNANSWERED, REASON_TRANSPORT, now);
                    return;
                }
                ws_unmask_in_place(&mut self.tx[ROOM..ROOM + s.ws_len], mask);
                self.conn_down(active, now);
                self.post_rest(id, s, true);
            }
        }
    }

    /// The REST fallback: the envelope, a sub-slice of the rendered post.
    fn post_rest(&mut self, id: u64, s: Sealed, resend: bool) {
        self.rt.stats.note_rest_fallback();
        let written = now_ns();
        if let Some(rec) = self.rt.pending.get_mut(id) {
            rec.conn = CONN_REST;
            rec.written_ns = written;
        }
        let res = self.http.post(&self.tx[s.env..s.env + s.env_len]);
        let now = now_ns();
        match res {
            Ok((status, range)) => {
                self.rt.stats.note_action_sent();
                self.rt
                    .on_rest_answer(id, status, &self.http.resp()[range], resend, now);
            }
            Err(e) => {
                let kind = if e.left_host || resend {
                    EV_UNANSWERED
                } else {
                    EV_REJECTED
                };
                self.rt.fail_all(id, kind, REASON_TRANSPORT, now);
            }
        }
    }

    /// Posts past their deadline: unanswered (E-5). A post that expired
    /// on the active socket with nothing read since it was written means
    /// the socket went silent: it is replaced at once, not after the
    /// keepalive's 45 s — and if a replacement is already under way, the
    /// silent socket is closed so the next posts go by REST rather than
    /// into it.
    fn expire(&mut self, now: u64) -> bool {
        let mut e = Expired {
            deadline_ns: 0,
            key: 0,
            kind: 0,
        };
        let mut worked = false;
        while self.rt.timers.pop_due(now, &mut e) {
            worked = true;
            if e.kind != TIMER_POST {
                continue;
            }
            let Some(rec) = self.rt.pending.get_mut(e.key) else {
                continue;
            };
            rec.timer = None;
            let (conn, written) = (rec.conn, rec.written_ns);
            self.rt.stats.note_post_timeout();
            self.rt
                .fail_all(e.key, EV_UNANSWERED, REASON_TRANSPORT, now);
            if conn < 2
                && self.mbb.active() == Some(conn)
                && self.conns[usize::from(conn)].last_rx_ns() <= written
            {
                if self.mbb.replace_pending() {
                    self.conn_down(conn, now);
                } else {
                    self.mbb.request_replace();
                }
                self.silent_replace = true;
            }
        }
        worked
    }

    /// The sweep (XH4d, XG-1): a cancel-all the engine asks for through
    /// the stats word, never through the command ring. A round reads the
    /// account's open orders, cancels every one of this gateway's slots
    /// by cloid, and settles before the next read; a read with none of
    /// ours left is `SWEEP_CLEAR`, [`SWEEP_ROUNDS`] rounds without one
    /// `SWEEP_STRANDED`. A newer request restarts it.
    fn sweep(&mut self, now: u64) -> bool {
        let req = self.rt.stats.sweep_requested();
        if req != self.sweep_seq {
            self.sweep_seq = req;
            self.sweep_round = 0;
            self.sweep_phase = SW_READ;
            self.rt.stats.set_sweep(req, SWEEP_WORKING);
        }
        match self.sweep_phase {
            SW_READ => {
                // The read's own room: a pending slot and a read's events.
                if self.rt.pending.live() + CLASSES > MAX_IN_FLIGHT {
                    return false;
                }
                self.rt.sweep_outcome = SWEEP_NONE;
                self.rt.counters.sweep_rounds += 1;
                let c = HlCmd::read_orders(SWEEP_SLOT, 0).at(now);
                self.admit_alone(&c, CLASS_READ, now);
                self.flush_open(now);
                self.sweep_phase = SW_WAIT;
                true
            }
            SW_WAIT => match self.rt.sweep_outcome {
                SWEEP_READ => {
                    self.rt.sweep_outcome = SWEEP_NONE;
                    self.sweep_cancel_ours(now);
                    true
                }
                SWEEP_FAILED => {
                    self.rt.sweep_outcome = SWEEP_NONE;
                    self.sweep_next_round(now);
                    true
                }
                _ => false,
            },
            SW_SETTLE if now >= self.sweep_until => {
                self.sweep_phase = SW_READ;
                true
            }
            _ => false,
        }
    }

    /// A sweep read came back: clear if none of ours rests, else cancel
    /// every one it can and settle.
    fn sweep_cancel_ours(&mut self, now: u64) {
        let n = self.rt.sweep_n.min(self.rt.sweep_hits.len());
        let mut mine = 0usize;
        let mut i = 0usize;
        while i < n {
            let h = self.rt.sweep_hits[i];
            i += 1;
            if h.slot >= 8 || self.slots & (1u8 << h.slot) == 0 {
                continue;
            }
            mine += 1;
            if h.asset == ASSET_UNKNOWN {
                self.rt.counters.sweep_unmapped += 1;
                continue;
            }
            if self.xg2_full() {
                // XG-2 binds the sweep too: the rest goes next round.
                continue;
            }
            self.rt.counters.sweep_cancels += 1;
            self.admit(&HlCmd::cancel(h.slot, h.client_oid, h.asset).at(now), now);
        }
        if mine == 0 {
            self.rt.stats.set_sweep(self.sweep_seq, SWEEP_CLEAR);
            self.sweep_phase = SW_IDLE;
            return;
        }
        self.flush_open(now);
        self.sweep_next_round(now);
    }

    /// Settle, then read again — or give up after the last round.
    fn sweep_next_round(&mut self, now: u64) {
        self.sweep_round += 1;
        if self.sweep_round >= SWEEP_ROUNDS {
            self.rt.stats.set_sweep(self.sweep_seq, SWEEP_STRANDED);
            self.sweep_phase = SW_IDLE;
            return;
        }
        self.sweep_until = now.saturating_add(SWEEP_SETTLE_NS);
        self.sweep_phase = SW_SETTLE;
    }

    /// Set the halt word (the first reason wins, XG-5) and tell the arm.
    fn halt(&mut self, reason: u64, now: u64) {
        if self.rt.stats.halt_reason() == HALT_NONE {
            self.rt.stats.halt(reason);
            emit_to(&mut self.rt.evt, &gateway_event(EV_HALT, reason, 0, now));
        }
    }
}

/// `host:port` → the first address. Blocking (getaddrinfo): boot, and a
/// re-resolve after repeated failed dials with no live socket.
fn resolve(host: &str, port: u16) -> Result<SocketAddr, GatewayErr> {
    (host, port)
        .to_socket_addrs()
        .map_err(|_| GatewayErr::Dns)?
        .next()
        .ok_or(GatewayErr::Dns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::ORDER_EVENT_REASON_BAD_ALO_PX;

    fn cmd(kind: u8, tif: Tif) -> HlCmd {
        let c = HlCmd::place(6, 1, 0, true, 1, 1, tif);
        match kind {
            CMD_MODIFY => c.replacing(9),
            CMD_CANCEL => HlCmd::cancel(6, 1, 0),
            _ => c,
        }
    }

    #[test]
    fn classes_never_mix_and_gtc_is_not_carried() {
        assert_eq!(class_of(&cmd(CMD_PLACE, Tif::Alo)), Some(CLASS_ALO));
        assert_eq!(class_of(&cmd(CMD_PLACE, Tif::Ioc)), Some(CLASS_TAKER));
        assert_eq!(
            class_of(&cmd(CMD_PLACE, Tif::Gtc)),
            None,
            "XH-3: ALO or IoC only"
        );
        assert_eq!(class_of(&cmd(CMD_MODIFY, Tif::Alo)), Some(CLASS_MODIFY));
        assert_eq!(
            class_of(&cmd(CMD_MODIFY, Tif::Ioc)),
            None,
            "a taker modify is refused"
        );
        assert_eq!(class_of(&cmd(CMD_CANCEL, Tif::Gtc)), Some(CLASS_CANCEL));
        let mut odd = cmd(CMD_PLACE, Tif::Alo);
        odd.kind = 9;
        assert_eq!(class_of(&odd), None);
        let mut bad_tif = cmd(CMD_PLACE, Tif::Alo);
        bad_tif.tif = 7;
        assert_eq!(class_of(&bad_tif), None, "an unknown tif is never a Gtc");
    }

    #[test]
    fn statuses_must_fit_their_class() {
        assert!(fits(CLASS_ALO as u8, ITEM_RESTING));
        assert!(fits(CLASS_MODIFY as u8, ITEM_FILLED));
        assert!(fits(CLASS_TAKER as u8, ITEM_ERROR));
        assert!(
            !fits(CLASS_ALO as u8, ITEM_SUCCESS),
            "a place is never a bare success"
        );
        assert!(fits(CLASS_CANCEL as u8, ITEM_SUCCESS));
        assert!(!fits(CLASS_CANCEL as u8, ITEM_RESTING));
    }

    #[test]
    fn the_coin_map_binds_and_refuses() {
        let mut m = CoinMap::new();
        assert!(m.insert(b"BTC", 0));
        assert!(m.insert(b"ETH", 1));
        assert!(!m.insert(b"", 2));
        assert!(!m.insert(b"A_NAME_LONGER_THAN_16", 3));
        assert_eq!(m.asset(b"ETH"), 1);
        assert_eq!(m.asset(b"BT"), ASSET_UNKNOWN);
        assert_eq!(m.asset(b"SOL"), ASSET_UNKNOWN);
        let mut full = CoinMap::new();
        let mut i = 0u32;
        while (i as usize) < COIN_SLOTS {
            assert!(full.insert(&[b'A' + (i % 26) as u8, b'0' + (i / 26) as u8], i));
            i += 1;
        }
        assert!(!full.insert(b"ONE_MORE", 99));
    }

    #[test]
    fn owners_and_update_keys() {
        let ours = crate::cloid::encode(6, 42);
        assert_eq!(owner(Some(ours)), (6, 42));
        assert_eq!(owner(None), (SLOT_FOREIGN, 0));
        assert_eq!(owner(Some([0u8; 16])), (SLOT_FOREIGN, 0));
        let a = OrderUpdate {
            oid: 5,
            status_ms: 10,
            kind: 2,
            ..OrderUpdate::default()
        };
        let b = OrderUpdate { sz_1e8: 1, ..a };
        let c = OrderUpdate { kind: 4, ..a };
        assert_ne!(
            update_key(&a),
            update_key(&b),
            "a partial fill is a new row"
        );
        assert_ne!(update_key(&a), update_key(&c));
        assert_ne!(update_key(&OrderUpdate::default()), 0);
    }

    #[test]
    fn an_answer_maps_each_status_onto_its_item() {
        let r = ItemRef::of(&HlCmd::place(6, 7, 3, false, 1, 1, Tif::Alo));
        let rest = ItemStatus {
            kind: ITEM_RESTING,
            oid: 99,
            ..ItemStatus::default()
        };
        let ev = answer_event(&r, &rest, 5, 11);
        assert_eq!(
            (ev.kind, ev.oid, ev.aux, ev.client_oid, ev.slot),
            (EV_ACK, 99, 5, 7, 6)
        );
        assert_eq!(ev.flags & EVF_RESTING, EVF_RESTING);
        assert_eq!(ev.flags & EVF_POST_ITEM, EVF_POST_ITEM);
        assert_eq!(ev.cmd_kind(), CMD_PLACE);
        assert_eq!(ev.flags & EVF_BUY, 0);
        let filled = ItemStatus {
            kind: ITEM_FILLED,
            oid: 100,
            total_sz_1e8: 3,
            avg_px_1e8: 4,
            ..ItemStatus::default()
        };
        let ev = answer_event(&r, &filled, 5, 11);
        assert_eq!((ev.kind, ev.sz_1e8, ev.px_1e8), (EV_ACK, 3, 4));
        assert_eq!(ev.flags & EVF_FILLED, EVF_FILLED);
        let err = ItemStatus {
            kind: ITEM_ERROR,
            reason: ORDER_EVENT_REASON_BAD_ALO_PX,
            ..ItemStatus::default()
        };
        let ev = answer_event(&r, &err, 5, 11);
        assert_eq!(
            (ev.kind, ev.reason),
            (EV_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX)
        );
    }

    /// A router with one post in flight: `n` ALO places, cancels, or
    /// leverage changes (a control post carries one).
    fn router_with_post(class: usize, n: usize) -> (Router, HlEngineEnd, u64) {
        let (eng, gw) = hl_duplex();
        let mut rt = Router::new(gw.evt, gw.stats, CoinMap::new());
        let id = 77u64;
        let rec = rt.pending.claim(id, 1).expect("claim");
        rec.class = class as u8;
        rec.conn = CONN_REST;
        rec.timer = None;
        rec.written_ns = 1;
        rec.n = n as u8;
        let mut i = 0usize;
        while i < n {
            let c = if class == CLASS_CANCEL {
                HlCmd::cancel(6, 10 + i as u64, 1)
            } else if class == CLASS_CONTROL {
                HlCmd::leverage(6, 10 + i as u64, 1, true, 3)
            } else {
                HlCmd::place(6, 10 + i as u64, 1, true, 1, 1, Tif::Alo)
            };
            rec.items[i] = ItemRef::of(&c);
            i += 1;
        }
        rt.inflight_items = n;
        (rt, eng, id)
    }

    fn drained(eng: &mut HlEngineEnd) -> Vec<ExecEvent> {
        let mut v = Vec::new();
        while let Some(e) = eng.evt.pop() {
            v.push(*e);
        }
        v
    }

    #[test]
    fn a_refused_re_send_is_in_doubt_never_rejected() {
        let refused = br#"{"status":"err","response":"Too many cumulative requests sent"}"#;
        let (mut rt, mut eng, id) = router_with_post(CLASS_ALO, 2);
        rt.on_rest_answer(id, 200, refused, true, 5);
        let ev = drained(&mut eng);
        assert_eq!(ev.len(), 2);
        assert!(ev
            .iter()
            .all(|e| e.kind == EV_UNANSWERED && e.reason == REASON_RATE_LIMIT));
        assert!(rt.pending.get(id).is_none(), "released");
        assert_eq!(rt.inflight_items, 0);

        let (mut rt, mut eng, id) = router_with_post(CLASS_ALO, 1);
        rt.on_rest_answer(id, 200, refused, false, 5);
        assert_eq!(
            drained(&mut eng)[0].kind,
            EV_REJECTED,
            "a first send's refusal is final"
        );

        let (mut rt, mut eng, id) = router_with_post(CLASS_ALO, 1);
        rt.on_rest_answer(id, 429, b"slow down", true, 5);
        assert_eq!(
            drained(&mut eng)[0].kind,
            EV_UNANSWERED,
            "a 429 on a re-send is in doubt"
        );

        let (mut rt, mut eng, id) = router_with_post(CLASS_ALO, 1);
        rt.on_rest_answer(id, 429, b"slow down", false, 5);
        let e = drained(&mut eng)[0];
        assert_eq!((e.kind, e.reason), (EV_REJECTED, REASON_RATE_LIMIT));

        let (mut rt, mut eng, id) = router_with_post(CLASS_ALO, 1);
        rt.on_rest_answer(id, 502, b"<html>bad gateway</html>", false, 5);
        let e = drained(&mut eng)[0];
        assert_eq!((e.kind, e.reason), (EV_UNANSWERED, REASON_TRANSPORT));
    }

    #[test]
    fn an_api_layer_error_or_an_ill_fitting_answer_is_in_doubt() {
        let (mut rt, mut eng, id) = router_with_post(CLASS_ALO, 1);
        rt.on_answer(id, b"Too many cumulative requests sent", false, false, 5);
        let e = drained(&mut eng)[0];
        assert_eq!((e.kind, e.reason), (EV_UNANSWERED, REASON_RATE_LIMIT));

        let (mut rt, mut eng, id) = router_with_post(CLASS_ALO, 1);
        let success =
            br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;
        rt.on_answer(id, success, true, false, 5);
        let e = drained(&mut eng)[0];
        assert_eq!((e.kind, e.reason), (EV_UNANSWERED, REASON_MALFORMED));
        assert_eq!(rt.counters.answers_mismatched, 1);

        let (mut rt, mut eng, id) = router_with_post(CLASS_CANCEL, 1);
        rt.on_answer(id, success, true, false, 5);
        let e = drained(&mut eng)[0];
        assert_eq!((e.kind, e.cmd_kind()), (EV_ACK, CMD_CANCEL));
        assert!(rt.healthy, "a matched answer moved data");

        let (mut rt, mut eng, id) = router_with_post(CLASS_ALO, 1);
        rt.on_answer(id + 1, success, true, false, 5);
        assert!(
            drained(&mut eng).is_empty(),
            "an answer to no post is dropped"
        );
        assert!(rt.pending.get(id).is_some());
    }

    #[test]
    fn the_first_snapshot_is_history_and_a_bad_frame_is_a_gap() {
        let (eng_end, gw) = hl_duplex();
        let mut eng = eng_end;
        let mut rt = Router::new(gw.evt, gw.stats, CoinMap::new());
        let snap = br#"{"channel":"userFills","data":{"isSnapshot":true,"user":"0x","fills":[{"coin":"ETH","px":"1","sz":"1","side":"B","time":1700,"oid":1,"tid":5},{"coin":"ETH","px":"1","sz":"1","side":"A","time":1900,"oid":2,"tid":6}]}}"#;
        rt.on_frame(snap, 1);
        let e = drained(&mut eng);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].kind, e[0].aux, e[0].venue_ms), (EV_SNAPSHOT, 2, 1900));
        assert!(rt.snapshot_absorbed);
        rt.on_frame(snap, 2);
        assert!(
            drained(&mut eng).is_empty(),
            "a later snapshot re-delivers nothing new"
        );
        rt.on_frame(
            br#"{"channel":"userFills","data":{"fills":[{"coin":"ETH"}]}}"#,
            3,
        );
        rt.on_frame(
            br#"{"channel":"orderUpdates","data":[{"status":"open"}]}"#,
            3,
        );
        let e = drained(&mut eng);
        assert_eq!(
            (e[0].kind, e[0].aux, e[1].kind, e[1].aux),
            (EV_GAP, GAP_FILLS_UNREADABLE, EV_GAP, GAP_UPDATES_UNREADABLE)
        );
    }

    #[test]
    fn control_commands_are_their_own_class_and_only_leverage_grows() {
        let lev = HlCmd::leverage(6, 1, 0, true, 3);
        let sched = HlCmd::schedule_cancel(6, 2, 1_789_000_005_000);
        assert_eq!(class_of(&lev), Some(CLASS_CONTROL));
        assert_eq!(class_of(&sched), Some(CLASS_CONTROL));
        assert_eq!(class_of(&HlCmd::noop(6, 3)), Some(CLASS_CONTROL));
        assert!(grows(CMD_PLACE) && grows(CMD_MODIFY) && grows(CMD_LEVERAGE));
        assert!(!grows(CMD_CANCEL) && !grows(CMD_SCHEDULE_CANCEL) && !grows(CMD_NOOP));
        assert!(growing(CLASS_CONTROL, CMD_LEVERAGE));
        assert!(!growing(CLASS_CONTROL, CMD_SCHEDULE_CANCEL));
        assert!(!growing(CLASS_CONTROL, CMD_NOOP));
        assert!(growing(CLASS_ALO, 0) && growing(CLASS_TAKER, 0) && growing(CLASS_MODIFY, 0));
        assert!(!growing(CLASS_CANCEL, CMD_LEVERAGE), "a cancel never grows");
        assert_eq!(
            (
                cmd_bits(CMD_PLACE),
                cmd_bits(CMD_MODIFY),
                cmd_bits(CMD_CANCEL)
            ),
            (CMD_PLACE, CMD_MODIFY, CMD_CANCEL)
        );
        assert_eq!(cmd_bits(CMD_LEVERAGE), 0);
        assert_eq!(
            cmd_bits(CMD_SCHEDULE_CANCEL),
            0,
            "5 & 3 would read as a place"
        );
        assert_eq!(ItemRef::of(&sched).event(EV_ACK, 0, 9, 1).cmd_kind(), 0);
    }

    #[test]
    fn one_dispatch_encodes_each_control_action_and_refuses_any_other_kind() {
        let mut buf = [0u8; 128];
        let mut w = ControlWire {
            kind: CMD_SCHEDULE_CANCEL,
            ..ControlWire::default()
        };
        let n = encode_control(&w, &mut buf, true).unwrap();
        assert_eq!(&buf[..n], br#"{"type":"scheduleCancel"}"#);
        w.time = Some(1_789_000_005_000);
        let n = encode_control(&w, &mut buf, true).unwrap();
        assert_eq!(
            &buf[..n],
            br#"{"type":"scheduleCancel","time":1789000005000}"#
        );
        assert!(encode_control(&w, &mut buf, false).is_ok());
        let lev = ControlWire {
            kind: CMD_LEVERAGE,
            is_cross: true,
            asset: 1,
            leverage: 3,
            time: None,
        };
        let n = encode_control(&lev, &mut buf, true).unwrap();
        assert_eq!(
            &buf[..n],
            br#"{"type":"updateLeverage","asset":1,"isCross":true,"leverage":3}"#
        );
        assert!(encode_control(&lev, &mut buf, false).is_ok());
        let noop = ControlWire {
            kind: CMD_NOOP,
            ..ControlWire::default()
        };
        let n = encode_control(&noop, &mut buf, true).unwrap();
        assert_eq!(&buf[..n], br#"{"type":"noop"}"#);
        let odd = ControlWire {
            kind: 9,
            ..ControlWire::default()
        };
        assert!(
            encode_control(&odd, &mut buf, false).is_err(),
            "never a noop by default"
        );
        assert!(encode_control(&odd, &mut buf, true).is_err());
    }

    #[test]
    fn a_control_answer_is_the_default_shape_a_refusal_or_in_doubt() {
        let default = br#"{"status":"ok","response":{"type":"default"}}"#;
        let (mut rt, mut eng, id) = router_with_post(CLASS_CONTROL, 1);
        rt.on_answer(id, default, true, false, 5);
        let e = drained(&mut eng);
        assert_eq!(e.len(), 1);
        assert_eq!(
            (
                e[0].kind,
                e[0].client_oid,
                e[0].asset,
                e[0].cmd_kind(),
                e[0].aux
            ),
            (EV_ACK, 10, 1, 0, id)
        );
        assert!(rt.healthy);
        assert!(rt.pending.get(id).is_none(), "released");
        assert_eq!(rt.inflight_items, 0);

        let refused = br#"{"status":"err","response":"Cannot set scheduled cancel time until enough volume traded."}"#;
        let (mut rt, mut eng, id) = router_with_post(CLASS_CONTROL, 1);
        rt.on_answer(id, refused, true, false, 5);
        assert_eq!(drained(&mut eng)[0].kind, EV_REJECTED, "the venue's no");
        let (mut rt, mut eng, id) = router_with_post(CLASS_CONTROL, 1);
        rt.on_answer(id, refused, true, true, 5);
        assert_eq!(
            drained(&mut eng)[0].kind,
            EV_UNANSWERED,
            "on a re-send the original may have landed"
        );

        let success =
            br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;
        let (mut rt, mut eng, id) = router_with_post(CLASS_CONTROL, 1);
        rt.on_answer(id, success, true, false, 5);
        let e = drained(&mut eng)[0];
        assert_eq!((e.kind, e.reason), (EV_UNANSWERED, REASON_MALFORMED));
        assert_eq!(
            rt.counters.answers_mismatched, 1,
            "items for a post without any"
        );

        let (mut rt, mut eng, id) = router_with_post(CLASS_CONTROL, 1);
        rt.on_answer(id, br#"{"status":"ok"}"#, true, false, 5);
        assert_eq!(drained(&mut eng)[0].reason, REASON_MALFORMED);

        let (mut rt, mut eng, id) = router_with_post(CLASS_CONTROL, 1);
        rt.on_rest_answer(id, 200, default, false, 5);
        assert_eq!(
            drained(&mut eng)[0].kind,
            EV_ACK,
            "the REST answer reads the same"
        );
    }
}
