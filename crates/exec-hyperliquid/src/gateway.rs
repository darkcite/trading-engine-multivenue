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
//! control actions `updateLeverage`, `scheduleCancel`, `noop` and — XMM
//! XH5c — `reserveRequestWeight`), the halt word is set (growth only — a
//! place, a modify, a leverage change, a weight purchase —
//! [`REASON_HALTED`]; cancels, `scheduleCancel` and `noop` still go), or
//! — growth only — the process's first `userFills` snapshot has not been
//! absorbed yet ([`REASON_NOT_READY`]): a fill of ours can never be
//! mistaken for history; or — growth only — the gateway runs at 80 % of a
//! rate share ([`REASON_THROTTLED`], XH4e, below).
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
//! E-3; its nonce from the agent's one stream, XH4h S3); its JSON is rendered once,
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
//! REST — the SAME envelope, rendered straight into the REST client's
//! `/exchange` window when the post is sealed (XMM HD2). A post whose
//! socket died under the write is re-sent by REST as the same signed
//! bytes (the mask XORs back), copied out of the TX buffer into that
//! window; the original may have landed, so only a per-item answer
//! settles it. The REST post blocks this thread for up to its deadline
//! — the fallback's cost, never the engine's.
//!
//! **Streams.** `userFills` → [`EV_FILL`] + [`EV_FILL_META`] (fee,
//! crossed), deduplicated by tid across connections and reconnects (LAW
//! E-5). The process's FIRST snapshot is history: its tids are admitted,
//! and [`EV_SNAPSHOT`] (rows, the latest fill's venue time) tells the arm
//! to read its boot reconciliation after it. A perps map emits none of its
//! rows; an outcome map emits its legs' rows of the last day and an hour,
//! flagged [`EVF_SNAPSHOT`], with no fee, for the arm to book once
//! (XH4i-3b-2b).
//! `orderUpdates` → status events, deduplicated (both sockets carry them
//! during a swap). A frame that does not scan, a socket that comes up
//! after a gap with no live socket, and one that replaces a socket that
//! had gone silent are [`EV_GAP`]: `orderUpdates` has no snapshot, so the
//! arm re-checks its open orders. Every fill, order-update, open-order and
//! position row, and every sweep cancel, names its asset through the
//! boot's [`CoinMap`], the coin read in place off the frame: perps by the
//! names the arm bound; on a map built [`CoinMap::with_outcomes`] (XH4i —
//! BIN15's gateway, slot 3, once XH4i-5 composes it), a HIP-4 leg by its
//! own `#<enc>` coin — never an id for a place (LAW E-4) — and the spot
//! read reports each leg the account holds ([`EV_BALANCE`], XH4i-2) by its
//! `+<enc>` (`asset::asset_of_balance_coin`), read in place the same way.
//!
//! **Rates (XH4e).** Every post is charged to a one-minute window of the
//! venue's IP weight — an action 1 + ⌊n/40⌋, a read its REST weight
//! (`clearinghouseState` and `spotClearinghouseState` 2, the others 20,
//! `userFillsByTime` 1 more per 20 rows it returns) — and every WebSocket
//! frame (posts, pings, pongs,
//! subscribes) to a window of its messages, each against this gateway's
//! share ([`IP_WEIGHT_SHARE`], [`WS_MSG_SHARE`]: the host's other HL
//! clients share the venue's per-IP limits). At 80 % of either share,
//! growth is refused unsent ([`REASON_THROTTLED`]); cancels, control
//! actions, reads and pings still go. The congestion word
//! (`GatewayStats::congestion`) says how close it runs: busy (60 % of a
//! share, 60 posts in flight, or socket answers averaging 1 s), throttled
//! — or, XMM XH5c-3, no socket Ready, every post going by REST: the member
//! holds — or rate-limited by the venue in the last 10 s. A REST post's
//! round trip (a fresh dial, a synchronous exchange) has its own series,
//! out of the socket's histograms, the average and the RTT word.
//!
//! **Copies** (house law): the msgpack encoding and the JSON rendering
//! (transforms, in place); the 64 B command out of its ring slot and each
//! 64 B event into its slot; the ≤ 125 B pong echo (rx → control
//! buffer); a re-send's envelope (≤ 16 KiB, out of the TX buffer into the
//! REST client's window — its signed bytes exist only there, XMM HD2);
//! boot-time moves (the dedupe rings, the RTT window and the TTL table,
//! the batch arrays, the coin table, the gateway itself); and core-net's
//! transport copies — kernel ↔ rustls, rustls ↔
//! the receive buffer and its compaction. **Allocations:** none of the
//! gateway's own after construction; rustls 0.23's buffered API allocates
//! one buffer per TLS record it seals or opens — more for an inbound
//! burst over 4 KiB (its receive window grows and shrinks), and once per
//! connection when its decrypted-record queue first grows to its
//! high-water mark; gate 96b pins the sockets' residue exactly, gate 101
//! the REST client's — and a dial or DNS re-resolve allocates.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use core_config::exec::EXEC_SLOTS;
use core_net::{
    ws_frame_in_place, ws_unmask_in_place, Backoff, KeepaliveCfg, TlsTransport, WsOpcode,
    WS_CLIENT_HDR_MAX,
};
use core_time::now_ns;
use core_types::{
    SlotMask, ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REASON_EXPIRED,
    ORDER_EVENT_REASON_NONE,
};
use exec_gateway::stats::{
    CONGESTION_BUSY, CONGESTION_NONE, CONGESTION_RATE_LIMITED, CONGESTION_THROTTLED,
    HALT_TIMER_FULL, SWEEP_CLEAR, SWEEP_STRANDED, SWEEP_WORKING,
};
use exec_gateway::{
    boxed_array, duplex, CmdRx, EngineEnd, EvtTx, Expired, GatewayEnd, GatewayStats, Idle, Mbb,
    MbbAction, Pending, RateWindow, RttWindow, SlotState, TimerId, TimerQueue, HALT_NONE,
};
use mio::{Events, Poll};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

use crate::action::{
    encode_batch_modify, encode_cancel_by_cloid, encode_noop, encode_order, encode_order_grouped,
    encode_reserve_weight, encode_schedule_cancel, encode_update_leverage, CancelByCloidWire,
    Grouping, ModifyWire, OrderWire, Tif, MAX_ACTION, MAX_ORDERS,
};
use crate::asset::{asset_of_balance_coin, asset_of_fill_coin};
use crate::budget::{rate_limit_request, scan_rate_limit};
use crate::cloid::{decode, Owner};
use crate::cmd::{
    HlCmd, CMD_CANCEL, CMD_FLAG_CROSS, CMD_LEVERAGE, CMD_MODIFY, CMD_NOOP, CMD_PLACE, CMD_READ_DAY,
    CMD_READ_LIMIT, CMD_READ_ORDERS, CMD_READ_SPOT, CMD_READ_STATE, CMD_RESERVE,
    CMD_SCHEDULE_CANCEL,
};
use crate::event::REASON_NOT_FOUND;
use crate::event::{
    never_left, reason_of_error, ExecEvent, ASSET_UNKNOWN, EVF_BUY, EVF_CMD_SHIFT, EVF_CROSSED,
    EVF_FILLED, EVF_POST_ITEM, EVF_RESTING, EVF_SNAPSHOT, EV_ACK, EV_BALANCE, EV_CANCELED, EV_FILL,
    EV_FILL_META, EV_GAP, EV_HALT, EV_OPEN_ORDER, EV_POSITION, EV_READ_DONE, EV_READ_FAILED,
    EV_REJECTED, EV_RESTING, EV_SNAPSHOT, EV_UNANSWERED, GAP_FILLS_UNREADABLE, GAP_RECONNECT,
    GAP_UPDATES_UNREADABLE, READ_DAY, READ_LIMIT, READ_ORDERS, READ_SPOT, READ_STATE,
    REASON_HALTED, REASON_LOCAL, REASON_MALFORMED, REASON_NOT_READY, REASON_RATE_LIMIT,
    REASON_SETTLEMENT, REASON_THROTTLED, REASON_TRANSPORT, SLOT_FOREIGN, SPOT_LEGS_READ,
};
use crate::gwconn::{GwConn, Phase};
use crate::http::{HlHttp, MAX_REQ_BODY};
use crate::json::{array_start, object_field, u64_field};
use crate::msgpack::MsgPackErr;
use crate::nonce::{now_ms, NonceSource, SharedNonce};
use crate::orderupd::{scan_order_updates, OrderUpdate};
use crate::recon::{
    fills_by_time_request, open_orders_request, perp_state_request, scan_open_orders,
    scan_perp_state, scan_spot_state, spot_state_request, OpenOrder, PerpPosition, SpotBalance,
    MAX_OPEN_ORDERS, MAX_SPOT_BALANCES,
};
use crate::request::{
    batch_modify_json, cancel_by_cloid_json, envelope_close, envelope_open, noop_json, order_json,
    order_json_grouped, reserve_weight_json, schedule_cancel_json, update_leverage_json,
    ws_info_open, ws_post_close, ws_post_open,
};
use crate::response::{
    scan, scan_items, HlResponse, ItemStatus, ItemsReply, ITEM_ERROR, ITEM_FILLED, ITEM_RESTING,
    ITEM_SUCCESS,
};
use crate::sign::{sign_action, Network, Vault};
use crate::ttl::{TtlTable, TTL_CANCEL_MARK};
use crate::userws::{scan_user_fills, sum_fill_rows, TidRing, UserFill, SNAPSHOT_RING};
use crate::wspost::{classify, PostKind, WsIn};

/// Command ring slots (engine → gateway).
pub const CMD_RING: usize = 1024;
/// Event ring slots (gateway → engine): four command rings (XG-2).
pub const EVT_RING: usize = 4096;
/// XH4i-3b-2b: an outcome map's first snapshot hands the arm its legs'
/// rows of the last day and an hour (a daily family's instance lives a
/// day) …
const SNAPSHOT_EMIT_MS: u64 = 25 * 3_600_000;
/// … at most a quarter of the event ring.
const SNAPSHOT_EMIT_MAX: usize = EVT_RING / 4;
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
// A re-send copies its envelope out of the TX buffer into the REST
// client's `/exchange` window (XMM HD2): every envelope fits.
const _: () = assert!(TX_CAP <= MAX_REQ_BODY);
const ROOM: usize = WS_CLIENT_HDR_MAX;
const GROUPING: &[u8] = b"na";

/// An ALO batch's grouping (XMM XH5d): its priority's `{"p": N}`, or
/// `"na"` when it bids none.
#[inline]
const fn grouping(p: u32) -> Grouping<'static> {
    if p == 0 {
        Grouping::Named(GROUPING)
    } else {
        Grouping::Priority(p)
    }
}

const CLASSES: usize = 6;
const CLASS_ALO: usize = 0;
const CLASS_TAKER: usize = 1;
const CLASS_MODIFY: usize = 2;
const CLASS_CANCEL: usize = 3;
/// One control action (leverage, scheduleCancel, noop, a weight purchase),
/// posted alone.
const CLASS_CONTROL: usize = 4;
/// One read (open orders, the perp sheet), posted alone, unsigned,
/// WebSocket only (XH4d).
const CLASS_READ: usize = 5;
/// Perp positions one state read may carry.
const MAX_POSITIONS: usize = 64;
/// XG-2: the event room a read's answer needs (its rows and its end).
const READ_RESERVE: usize = MAX_OPEN_ORDERS + 1;
const _: () = assert!(MAX_POSITIONS <= MAX_OPEN_ORDERS);
// XH4i-2: a spot read's legs (one `EV_BALANCE` a sheet row at most) and
// its end fit the same room: fewer rows than the room.
const _: () = assert!(MAX_SPOT_BALANCES < READ_RESERVE);
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
/// XH4e — this gateway's share of the venue's IP weight a minute (the
/// venue allows 1 200 per IP; the blocking arm and the ingress share it).
pub const IP_WEIGHT_SHARE: u32 = 400;
/// XH4e — its share of the venue's WebSocket messages a minute (2 000 per
/// IP).
pub const WS_MSG_SHARE: u32 = 1_000;
/// **XMM XH5b-2 — this gateway's own halt reasons** (`exec_gateway`'s
/// registry: a venue numbers its own from `HALT_VENUE_BASE`). Slot 6's
/// session anchor could not be stored ([`crate::anchor::AnchorKeeper`]): the
/// slot never quotes on an anchor a restart would lose (E7).
pub const HALT_ANCHOR_UNKEPT: u64 = exec_gateway::stats::HALT_VENUE_BASE;
/// XH4e — growth is refused unsent at this % of either share.
const THROTTLE_PCT: u64 = 80;
/// XH4e — the congestion word reads busy from this % of either share …
const BUSY_PCT: u64 = 60;
/// … or with this many posts in flight (of [`MAX_IN_FLIGHT`]) …
const BUSY_IN_FLIGHT: usize = 60;
/// … or once the answers take this long on average.
const BUSY_RTT_NS: u64 = 1_000_000_000;
/// XH4e — a venue rate-limit answer marks the word this long.
const RATE_LIMITED_NS: u64 = 10_000_000_000;
/// XH4e — a read's IP weight: the venue's REST weights (its weighting of
/// WebSocket-posted info is unmeasured), `clearinghouseState` and
/// `spotClearinghouseState` (XH5b-1) 2, the others 20.
const READ_WEIGHT: u32 = 20;
const STATE_READ_WEIGHT: u32 = 2;

/// A read command's IP weight (see [`READ_WEIGHT`]).
#[inline(always)]
const fn read_ip_weight(kind: u8) -> u32 {
    if kind == CMD_READ_STATE || kind == CMD_READ_SPOT {
        STATE_READ_WEIGHT
    } else {
        READ_WEIGHT
    }
}

/// XH4f — the IP weight a minute an arm's periodic reads spend: the sheet
/// and the orders every `recon_ms` — and the spot sheet with them when
/// `spot` (an account of the slot's own, XH5b-1) — the budget every
/// `limit_ms` (the XH4e amendment's arithmetic, pinned where an arm is
/// composed). Zero periods read as unbounded.
#[must_use]
pub const fn read_weight_per_min(recon_ms: u64, limit_ms: u64, spot: bool) -> u64 {
    if recon_ms == 0 || limit_ms == 0 {
        return u64::MAX;
    }
    let mut period = read_ip_weight(CMD_READ_STATE) + read_ip_weight(CMD_READ_ORDERS);
    if spot {
        period += read_ip_weight(CMD_READ_SPOT);
    }
    (60_000 / recon_ms) * period as u64
        + (60_000 / limit_ms) * read_ip_weight(CMD_READ_LIMIT) as u64
}
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
    /// arms can share an account, never a cloid namespace. A [`SlotMask`]
    /// since S16-4 (sixteen slots).
    pub slots: SlotMask,
    /// A post's answer deadline ([`POST_TIMEOUT_NS`] in production).
    pub post_timeout_ns: u64,
    /// The idle policy.
    pub idle: Idle,
    /// XH4e: this gateway's share of the venue's IP weight a minute
    /// ([`IP_WEIGHT_SHARE`] in production; > 0).
    pub ip_weight_per_min: u32,
    /// XH4e: its share of the venue's WebSocket messages a minute
    /// ([`WS_MSG_SHARE`] in production; > 0).
    pub ws_msgs_per_min: u32,
}

impl GatewayCfg {
    /// XH4f: one arm's gateway in production — the post timeout, the idle
    /// policy and the rate shares at their defaults, the slot mask that
    /// arm's one slot.
    #[must_use]
    pub fn production(
        host: String,
        tls: Arc<ClientConfig>,
        network: Network,
        master: [u8; 20],
        slot: u8,
    ) -> Self {
        // S16-4: a `SlotMask` names all sixteen slots, so slot 11 is bit
        // 11 — never `& 7`'s slot 3 (BIN15's). Every caller passes its
        // member's slot, which `HlGatewayArm::new` bounds. Past the mask's
        // width a release build's `<<` would wrap (19 → bit 3, BIN15's):
        // `checked_shl` gives such a slot the EMPTY mask instead — a
        // gateway that refuses every command, fail-closed.
        debug_assert!(
            usize::from(slot) < EXEC_SLOTS,
            "a gateway's slot is a router slot"
        );
        Self {
            host,
            port: crate::config::HTTPS_PORT,
            tls,
            network,
            master,
            slots: (1 as SlotMask).checked_shl(u32::from(slot)).unwrap_or(0),
            post_timeout_ns: POST_TIMEOUT_NS,
            idle: Idle::new(
                exec_gateway::idle::SPIN_POLLS_DEFAULT,
                exec_gateway::idle::PARK_NS_DEFAULT,
            ),
            ip_weight_per_min: IP_WEIGHT_SHARE,
            ws_msgs_per_min: WS_MSG_SHARE,
        }
    }
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
    /// XH4e: a rate share of 0 — the gateway could grow nothing.
    Rates,
}

/// Coins the gateway resolves to asset ids for the streams and the sweep
/// (perps: the arm's `/info meta` binding, XH4c). An unknown coin is
/// [`ASSET_UNKNOWN`]; the arm still has the client id.
///
/// **XH4i-1 — outcome legs by their own names.** A map built
/// [`with_outcomes`](Self::with_outcomes) — for BIN15's gateway (slot 3),
/// composed in XH4i-5 — also resolves a HIP-4 leg's `#<enc>` to
/// `ASSET_BASE + enc`, read off the venue's own coin name
/// (`asset::asset_of_fill_coin`, S7-L1). The spelling is measured on
/// testnet in all three places the gateway reads: `userFills`
/// (2026-09-15), `orderUpdates` and `frontendOpenOrders` (2026-09-19,
/// `#154150`; the vault's `outcome/data/testnet-exec-2026-09-19`, the
/// watch log and phase G's sweep). That id names an event's leg and a
/// sweep's cancel, never a place: LAW E-4 binds an order's id from a roll
/// event, in the arm. A perps-only map (slot 6's) leaves every `#` coin
/// unknown, as before.
///
/// **XH4i-2:** on such a map the spot read also reports each leg the
/// account holds — its `+<enc>` row on the spot sheet — as
/// [`EV_BALANCE`], and marks its end ([`SPOT_LEGS_READ`]): an outcome arm
/// tells a sheet read for legs from one that was not.
#[derive(Clone)]
pub struct CoinMap {
    names: [[u8; COIN_MAX]; COIN_SLOTS],
    lens: [u8; COIN_SLOTS],
    assets: [u32; COIN_SLOTS],
    n: usize,
    /// XH4i-1: a `#<enc>` coin resolves to its outcome leg; XH4i-2: the
    /// spot read reports the held `+<enc>` legs.
    outcomes: bool,
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
            outcomes: false,
        }
    }

    /// XH4i-1: empty, and naming HIP-4 outcome legs by their `#<enc>`
    /// coins (above). Perps still bind by name ([`insert`](Self::insert)).
    #[must_use]
    pub const fn with_outcomes() -> Self {
        let mut m = Self::new();
        m.outcomes = true;
        m
    }

    /// XH4i-3b-2b: whether the map names outcome legs (slot 3's gateway).
    #[must_use]
    pub const fn outcomes(&self) -> bool {
        self.outcomes
    }

    /// Bind `coin` to `asset`. `false`: the map is full or the name too
    /// long (boot refuses) — or, on an outcome map, a `#` name: a leg is
    /// named by its coin alone, and a binding would never be read.
    pub fn insert(&mut self, coin: &[u8], asset: u32) -> bool {
        if self.n == COIN_SLOTS
            || coin.is_empty()
            || coin.len() > COIN_MAX
            || (self.outcomes && coin.first() == Some(&b'#'))
        {
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

    /// The asset `coin` is bound to — on an outcome map, a `#<enc>` leg's
    /// own id — or [`ASSET_UNKNOWN`].
    #[must_use]
    pub fn asset(&self, coin: &[u8]) -> u32 {
        if self.outcomes && coin.first() == Some(&b'#') {
            // No perp's name starts with '#': the leg's name is its id.
            return asset_of_fill_coin(coin).unwrap_or(ASSET_UNKNOWN);
        }
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
/// ones); published at its heartbeat for `/metrics` ([`pw`], XH4f).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GatewayCounters {
    /// Fills emitted.
    pub fills: u64,
    /// `userFills` frames that did not scan.
    pub fills_scan_failed: u64,
    /// Rows of the first snapshot, absorbed as history.
    pub snapshot_rows: u64,
    /// XH4i-5: an outcome map's first-snapshot leg rows past its cap, not
    /// emitted — the arm's ledger misses them, and its reconciliation's
    /// drift halts the slot.
    pub snapshot_kept: u64,
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
    /// in the coin map (XH4d) — on an outcome map, nor a `#<enc>` leg
    /// (XH4i-1).
    pub sweep_unmapped: u64,
    /// Growing commands refused unsent at 80 % of a rate share (XH4e).
    pub throttled: u64,
    /// TTL cancels sent (XH4f, D9).
    pub ttl_cancels: u64,
    /// Orders whose TTL the full table could not keep (XH4f).
    pub ttl_full: u64,
}

/// XH4f (D11) — the words this gateway publishes at its heartbeat
/// ([`GatewayStats::publish`]) for its arm's report: its own counters, then
/// its rate windows' sums. One numbering, written here by name and read by
/// the arm by name.
pub mod pw {
    /// [`super::GatewayCounters::fills`].
    pub const FILLS: usize = 0;
    /// [`super::GatewayCounters::fills_scan_failed`].
    pub const FILLS_SCAN_FAILED: usize = 1;
    /// [`super::GatewayCounters::updates`].
    pub const UPDATES: usize = 2;
    /// [`super::GatewayCounters::updates_ignored`].
    pub const UPDATES_IGNORED: usize = 3;
    /// [`super::GatewayCounters::updates_scan_failed`].
    pub const UPDATES_SCAN_FAILED: usize = 4;
    /// [`super::GatewayCounters::answers_unreadable`].
    pub const ANSWERS_UNREADABLE: usize = 5;
    /// [`super::GatewayCounters::answers_mismatched`].
    pub const ANSWERS_MISMATCHED: usize = 6;
    /// [`super::GatewayCounters::ws_errors`].
    pub const WS_ERRORS: usize = 7;
    /// [`super::GatewayCounters::local_refusals`].
    pub const LOCAL_REFUSALS: usize = 8;
    /// [`super::GatewayCounters::cancels_untimed`].
    pub const CANCELS_UNTIMED: usize = 9;
    /// [`super::GatewayCounters::dials`].
    pub const DIALS: usize = 10;
    /// [`super::GatewayCounters::conn_drops`].
    pub const CONN_DROPS: usize = 11;
    /// [`super::GatewayCounters::sweep_rounds`].
    pub const SWEEP_ROUNDS: usize = 12;
    /// [`super::GatewayCounters::sweep_unmapped`].
    pub const SWEEP_UNMAPPED: usize = 13;
    /// [`super::GatewayCounters::throttled`].
    pub const THROTTLED: usize = 14;
    /// [`super::GatewayCounters::ttl_cancels`].
    pub const TTL_CANCELS: usize = 15;
    /// [`super::GatewayCounters::ttl_full`].
    pub const TTL_FULL: usize = 16;
    /// The IP-weight window's sum (the last minute).
    pub const RATE_IP: usize = 17;
    /// The WebSocket-message window's sum (the last minute).
    pub const RATE_WS: usize = 18;
    /// Words published.
    pub const N: usize = 19;
    // The last word and the count move together, inside the granule.
    const _: () = assert!(RATE_WS + 1 == N && N <= exec_gateway::stats::PUBLISHED_WORDS);
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
    /// A modify's replaced id; a day read's start time, ms (XH4e).
    prev: u64,
    /// The engine's push stamp (an order's push-to-write latency); a day
    /// read's end time, ms (XH4e: a read feeds no latency).
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

/// An open batch: its post id, open order, items, the smallest
/// venue-expiry offset among them, and (an ALO batch's, XMM XH5d) the
/// priority its action's grouping bids — 0 `"na"`.
#[derive(Copy, Clone, Default)]
struct BatchHead {
    id: u64,
    seq: u64,
    n: usize,
    min_exp: u16,
    p: u32,
}

/// A control action's arguments, resolved once at admission.
#[derive(Copy, Clone, Default)]
struct ControlWire {
    kind: u8,
    is_cross: bool,
    asset: u32,
    /// The leverage (`updateLeverage`), or the weight a purchase buys
    /// (`reserveRequestWeight`, XMM XH5c).
    arg: u32,
    /// `scheduleCancel`'s time; `None` clears.
    time: Option<u64>,
}

/// A control action into `dst`: its msgpack, or (`json`) its JSON twin —
/// one dispatch for both, so the signed bytes and the sent bytes can never
/// name different actions. A kind with no action is refused (the post is
/// refused locally; `class_of` admits none), never sent as a `noop`.
fn encode_control(c: &ControlWire, dst: &mut [u8], json: bool) -> Result<usize, MsgPackErr> {
    match (c.kind, json) {
        (CMD_LEVERAGE, false) => encode_update_leverage(dst, c.asset, c.is_cross, c.arg),
        (CMD_LEVERAGE, true) => update_leverage_json(dst, c.asset, c.is_cross, c.arg),
        (CMD_RESERVE, false) => encode_reserve_weight(dst, u64::from(c.arg)),
        (CMD_RESERVE, true) => reserve_weight_json(dst, u64::from(c.arg)),
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

/// Where a sealed post sits: in the TX buffer, or — sealed for REST — in
/// the REST client's `/exchange` window.
#[derive(Copy, Clone)]
struct Sealed {
    /// The envelope (the REST body): `tx[env..env + env_len]`, or
    /// `http.body_mut()[..env_len]` when `rest`.
    env: usize,
    env_len: usize,
    /// The WebSocket post: `tx[ROOM..ROOM + ws_len]` (0 when `rest`).
    ws_len: usize,
    /// XMM HD2: no socket was Ready when the post was sealed, so the
    /// envelope alone was rendered straight into the REST window — no
    /// WebSocket head it would never be sent with, and no copy.
    rest: bool,
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

/// A command that can add exposure or spend: a place, a modify, a leverage
/// change — a reduce-only place included (XH1: no member relies on the
/// bit for an exit) — or a weight purchase (XMM XH5c: a halted, unready or
/// throttled slot buys nothing). The halt and the snapshot gate refuse it,
/// and XG-3 will not send it untimed; cancels, `scheduleCancel` and `noop`
/// always go.
#[inline(always)]
const fn grows(kind: u8) -> bool {
    kind == CMD_PLACE || kind == CMD_MODIFY || kind == CMD_LEVERAGE || kind == CMD_RESERVE
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

/// A read command's `EV_READ_*` code.
#[inline(always)]
const fn read_code(kind: u8) -> u8 {
    match kind {
        CMD_READ_ORDERS => READ_ORDERS,
        CMD_READ_LIMIT => READ_LIMIT,
        CMD_READ_DAY => READ_DAY,
        CMD_READ_SPOT => READ_SPOT,
        _ => READ_STATE,
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

/// S16-4: is `slot` one of the gateway's `slots`? A byte at or past
/// `EXEC_SLOTS` — [`SWEEP_SLOT`], [`SLOT_FOREIGN`], a nonsense one — never
/// is; the bound goes first, so the shift is exact (a `u8` mask's
/// `1 << 11` would alias slot 11 onto BIN15's slot 3 in a release build).
/// The one test `HlGateway::admit` and `HlGateway::sweep_cancel_ours`
/// share.
#[inline(always)]
fn owns_slot(slots: SlotMask, slot: u8) -> bool {
    usize::from(slot) < EXEC_SLOTS && slots & ((1 as SlotMask) << slot) != 0
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

/// XH4f (D9): whether a TTL cancel's refusal uses up one of its tries — a
/// venue refusal does, account-level ones included (one the venue repeats
/// every time stays bounded); one that never left this host
/// ([`never_left`]) or met the venue's rate limit (not processed) does not.
#[inline]
const fn ttl_try_counts(reason: u8) -> bool {
    !never_left(reason) && reason != REASON_RATE_LIMIT
}

/// The event an answer's status makes of an item.
#[inline]
fn answer_event(r: &ItemRef, s: &ItemStatus, id: u64, now_ns: u64) -> ExecEvent {
    // XH4f (D9): a TTL cancel's success reads EXPIRED — the arm closes the
    // order as the TTL's.
    let why = if r.kind == CMD_CANCEL && r.prev == TTL_CANCEL_MARK {
        ORDER_EVENT_REASON_EXPIRED
    } else {
        ORDER_EVENT_REASON_NONE
    };
    let mut ev = r.event(EV_ACK, why, id, now_ns);
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
    /// XH5b-1: the spot sheet's rows (8 KiB, boxed once).
    spot_rows: Box<[SpotBalance; MAX_SPOT_BALANCES]>,
    /// The sweep read's orders of ours, and its outcome (`SWEEP_*`).
    sweep_hits: Box<[SweepHit; MAX_OPEN_ORDERS]>,
    sweep_n: usize,
    sweep_outcome: u8,
    /// XH4e: the post RTT's moving average (1/8 per answer) and the last
    /// venue rate-limit answer — the congestion word's inputs.
    rtt_ewma_ns: u64,
    rate_limited_ns: u64,
    /// XH4e: IP weight answers added (`userFillsByTime`: 1 per 20 rows),
    /// charged at the gateway's next turn.
    read_weight: u32,
    /// XH4f: the recent answered posts' round trips — the member's pull
    /// window, published with the heartbeat.
    rtt: Box<RttWindow>,
    /// XH4f (D9): the orders the gateway cancels at their TTL.
    ttl: Box<TtlTable>,
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
            // COPY: two 64 KiB dedupe rings, and (XH4f) the 1 088 B RTT
            // window and the 3 088 B TTL table (both const-asserted), built
            // on the stack and moved to the heap, once at boot — rejected:
            // an unsafe in-place initialisation of types whose fields are
            // private.
            tids: Box::new(TidRing::new()),
            seen_updates: Box::new(TidRing::new()),
            rtt: Box::new(RttWindow::new()),
            ttl: Box::new(TtlTable::new()),
            snapshot_absorbed: false,
            coins,
            counters: GatewayCounters::default(),
            inflight_items: 0,
            answered: [0; 2],
            healthy: false,
            open_rows: boxed_array(OpenOrder::default()),
            positions: boxed_array(PerpPosition::default()),
            spot_rows: boxed_array(SpotBalance::default()),
            sweep_hits: boxed_array(SweepHit::default()),
            sweep_n: 0,
            sweep_outcome: SWEEP_NONE,
            rtt_ewma_ns: 0,
            rate_limited_ns: 0,
            read_weight: 0,
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
                    let r = &rec.items[i];
                    let ev = answer_event(r, &self.items[i], id, now);
                    if ev.reason == REASON_RATE_LIMIT {
                        self.rate_limited_ns = now;
                    }
                    // XH4f (D9): a cancel that succeeded — the arm's or the
                    // TTL's — ends the TTL. A not-found ends nothing: about
                    // an order still in flight on another socket it does not
                    // mean gone (the row waits for the order's end; a late
                    // landing sends the TTL cancel again). A TTL cancel the
                    // venue refused otherwise goes again — any refusal but
                    // the rate limit counts toward its tries — so the TTL holds
                    // through an engine stall past its first try. A refused
                    // place or modify never rested; a landed modify ends its
                    // original (its replacement inherited the deadline).
                    if r.kind == CMD_CANCEL {
                        if ev.kind == EV_ACK {
                            let _ = self.ttl.forget(r.slot, r.client_oid);
                        } else if r.prev == TTL_CANCEL_MARK {
                            if ev.reason == REASON_NOT_FOUND {
                                self.ttl.missed(r.slot, r.client_oid);
                            } else {
                                let counted = ttl_try_counts(ev.reason);
                                self.ttl.retry(r.slot, r.client_oid, now, counted);
                            }
                        }
                    } else if ev.kind == EV_REJECTED {
                        let _ = self.ttl.forget(r.slot, r.client_oid);
                    } else if r.kind == CMD_MODIFY {
                        let _ = self.ttl.forget(r.slot, r.prev);
                    }
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
        let ok = match item.kind {
            CMD_READ_ORDERS => self.read_orders(&item, body, now),
            CMD_READ_LIMIT => self.read_limit(&item, body, now),
            CMD_READ_DAY => self.read_day(&item, body, now),
            CMD_READ_SPOT => self.read_spot(&item, body, now),
            _ => self.read_state(&item, body, now),
        };
        if ok {
            self.counters.reads += 1;
            self.healthy = true;
        } else {
            self.counters.reads_failed += 1;
            if item.slot == SWEEP_SLOT {
                self.sweep_outcome = SWEEP_FAILED;
            } else {
                emit_to(
                    &mut self.evt,
                    &read_end(&item, EV_READ_FAILED, read_code(item.kind), 0, now),
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

    /// XH5b-1 — `{"type":"spotClearinghouseState","data":{…}}` → one
    /// `EV_READ_DONE` (`READ_SPOT`): `px_1e8` = the USDC `total` (USD ×1e8),
    /// `sz_1e8` = 1 when the sheet lists USDC — 0 when it lists none, which
    /// the arm refuses as no worth (`recon::account_view`'s rule: nothing to
    /// anchor) — `aux` = the rows, scanned in place into a fixed table.
    /// **XH4i-2:** on an outcome map, first one [`EV_BALANCE`] per `+<enc>`
    /// row with a holding (`asset::asset_of_balance_coin`, or
    /// [`ASSET_UNKNOWN`]: still a leg, which an outcome arm counts unseen,
    /// XH4i-4), and the end's `oid` counts them and its `asset` is
    /// [`SPOT_LEGS_READ`]. `false`: it does not scan (more rows than the
    /// table holds included) — before any row goes out.
    fn read_spot(&mut self, item: &ItemRef, body: &[u8], now: u64) -> bool {
        let Some((s, e)) = object_field(body, b"\"data\"") else {
            return false;
        };
        let data = &body[s..e];
        let Ok(n) = scan_spot_state(data, &mut self.spot_rows[..]) else {
            return false;
        };
        let n = n.min(self.spot_rows.len());
        let outcomes = self.coins.outcomes;
        let mut usdc_1e8 = 0i64;
        let mut listed = false;
        let mut legs = 0u64;
        let mut i = 0usize;
        while i < n {
            let b = &self.spot_rows[i];
            let coin = b.coin.of(data);
            if coin == b"USDC" {
                usdc_1e8 = b.total_1e8;
                listed = true;
            } else if outcomes && b.total_1e8 != 0 && coin.first() == Some(&b'+') {
                // XH4i-2: a leg the account holds — a settled husk holds
                // nothing (`recon::account_view`'s rule).
                let ev = ExecEvent {
                    ts_ns: now,
                    client_oid: item.client_oid,
                    px_1e8: b.entry_ntl_1e8,
                    sz_1e8: b.total_1e8,
                    asset: asset_of_balance_coin(coin).unwrap_or(ASSET_UNKNOWN),
                    kind: EV_BALANCE,
                    slot: item.slot,
                    ..ExecEvent::default()
                };
                emit_to(&mut self.evt, &ev);
                legs += 1;
            }
            i += 1;
        }
        let mut end = read_end(item, EV_READ_DONE, READ_SPOT, n as u64, now);
        end.px_1e8 = usdc_1e8;
        end.sz_1e8 = i64::from(listed);
        end.oid = legs;
        if outcomes {
            end.asset = SPOT_LEGS_READ;
        }
        emit_to(&mut self.evt, &end);
        true
    }

    /// `userRateLimit` (XH4e) → one `EV_READ_DONE` (`READ_LIMIT`): `aux` =
    /// `nRequestsUsed`, `px_1e8` = `cumVlm` (USD ×1e8), `sz_1e8` =
    /// `nRequestsSurplus` (`budget::scan_rate_limit`'s rules). `false`: it
    /// does not scan.
    fn read_limit(&mut self, item: &ItemRef, body: &[u8], now: u64) -> bool {
        let Some((used, vlm_1e6, surplus)) = scan_rate_limit(body) else {
            return false;
        };
        let mut end = read_end(item, EV_READ_DONE, READ_LIMIT, used, now);
        end.px_1e8 = vlm_1e6.saturating_mul(100);
        end.sz_1e8 = i64::try_from(surplus).unwrap_or(i64::MAX);
        emit_to(&mut self.evt, &end);
        true
    }

    /// `userFillsByTime` (XH4e) → one `EV_READ_DONE` (`READ_DAY`): `aux` =
    /// rows, `px_1e8` = Σ |px × sz| of the read slot's rows (USD ×1e8),
    /// `sz_1e8` = the rows whose time ran backward, `venue_ms` = the latest
    /// row's time — summed in place, no row table. The venue weighs the
    /// answer 1 more per 20 rows. `false`: it does not scan.
    fn read_day(&mut self, item: &ItemRef, body: &[u8], now: u64) -> bool {
        let Ok((rows, sum_1e8, last_ms, back)) = sum_fill_rows(body, item.slot) else {
            return false;
        };
        self.read_weight = self
            .read_weight
            .saturating_add(u32::try_from(rows / 20).unwrap_or(u32::MAX));
        let mut end = read_end(item, EV_READ_DONE, READ_DAY, rows, now);
        end.px_1e8 = sum_1e8;
        end.sz_1e8 = i64::try_from(back).unwrap_or(i64::MAX);
        end.venue_ms = last_ms;
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
        if reason == REASON_RATE_LIMIT {
            self.rate_limited_ns = now;
        }
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
            let r = &rec.items[i];
            // XH4f (D9): a TTL cancel refused or unanswered goes again — a
            // venue refusal counts toward its tries; a doubt, one that never
            // left this host and the rate limit never do (the
            // arm's own cancel leaves the TTL running); a refused place or
            // modify never rested; an unanswered one waits for its order's
            // end row.
            if r.kind == CMD_CANCEL {
                if r.prev == TTL_CANCEL_MARK {
                    let counted = kind == EV_REJECTED && ttl_try_counts(reason);
                    self.ttl.retry(r.slot, r.client_oid, now, counted);
                }
            } else if kind == EV_REJECTED {
                let _ = self.ttl.forget(r.slot, r.client_oid);
            }
            emit_to(&mut self.evt, &r.event(kind, reason, id, now));
            i += 1;
        }
        self.finish(id, now);
    }

    /// Release post `id`: its deadline, its connection's count, its
    /// latency sample — XMM XH5c-3: a REST post's in its own series alone.
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
            if conn == CONN_REST {
                // XMM XH5c-3: a fresh dial and a synchronous exchange (the
                // boot's dead-man clear, a fallback with no socket Ready) —
                // not the socket's path: its own series alone.
                self.stats.rest_rtt.record(rtt);
            } else {
                if usize::from(class) == CLASS_CANCEL {
                    self.stats.cancel_rtt.record(rtt);
                } else {
                    self.stats.post_rtt.record(rtt);
                }
                // XH4e: the congestion word's moving average.
                self.rtt_ewma_ns = self.rtt_ewma_ns - self.rtt_ewma_ns / 8 + rtt / 8;
                // XH4f: the member's pull window.
                self.rtt.record(now, rtt);
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
                latest = latest.max(self.fills[i].time_ms);
                i += 1;
            }
            self.counters.snapshot_rows += n as u64;
            // XH4i-3b-2b (A-S7-72): on an outcome map the legs' history is
            // the arm's to book, once — the blocking arm books its snapshot
            // too — so its rows go out, flagged, with no fee (an earlier
            // session's): the legs' rows of the last day and an hour, at
            // most a quarter of the event ring (the rest counted). Every tid
            // is admitted here; only a freshly admitted one goes out — never
            // a row the stream already sent. A perps map's rows stay the
            // boot reconciliation's.
            let outcomes = self.coins.outcomes();
            let since = latest.saturating_sub(SNAPSHOT_EMIT_MS);
            let mut sent = 0usize;
            let mut i = 0usize;
            while i < n {
                let fresh = self.tids.admit(self.fills[i].tid);
                if outcomes && fresh && self.fills[i].time_ms >= since {
                    let a = self.coins.asset(self.fills[i].coin.of(frame));
                    if a != ASSET_UNKNOWN && a >= crate::asset::ASSET_BASE {
                        if sent < SNAPSHOT_EMIT_MAX {
                            self.emit_fill_row(i, a, true, false, now);
                            sent += 1;
                        } else {
                            self.counters.snapshot_kept += 1;
                        }
                    }
                }
                i += 1;
            }
            self.counters.fills += sent as u64;
            emit_to(
                &mut self.evt,
                &gateway_event(EV_SNAPSHOT, n as u64, latest, now),
            );
            return;
        }
        let wall_ns = now_ms().saturating_mul(1_000_000);
        let mut i = 0usize;
        while i < n {
            if self.tids.admit(self.fills[i].tid) {
                if !is_snapshot {
                    let ms = self.fills[i].time_ms;
                    self.stats
                        .fill_venue_to_local
                        .record(wall_ns.saturating_sub(ms.saturating_mul(1_000_000)));
                }
                let a = self.coins.asset(self.fills[i].coin.of(frame));
                self.emit_fill_row(i, a, is_snapshot, true, now);
                self.counters.fills += 1;
            }
            i += 1;
        }
    }

    /// One `userFills` row out on `asset` (its coin, as the map names it):
    /// the fill, and its fee when `meta` (`EVF_SNAPSHOT` on a snapshot's
    /// row) — the live rows' builder, and an outcome map's first
    /// snapshot's (XH4i-3b-2b).
    #[inline]
    fn emit_fill_row(&mut self, i: usize, asset: u32, snapshot: bool, meta: bool, now: u64) {
        let f = &self.fills[i];
        let (slot, client_oid) = owner(f.cloid);
        let mut flags = if f.is_buy { EVF_BUY } else { 0 };
        if f.crossed {
            flags |= EVF_CROSSED;
        }
        if snapshot {
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
            asset,
            kind: EV_FILL,
            reason: if f.is_settlement {
                REASON_SETTLEMENT
            } else {
                0
            },
            slot,
            flags,
        };
        emit_to(&mut self.evt, &fill);
        if meta {
            emit_to(
                &mut self.evt,
                &ExecEvent {
                    kind: EV_FILL_META,
                    px_1e8: f.fee_1e8,
                    reason: 0,
                    ..fill
                },
            );
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
                // XH4f (D9): an order's end drops its TTL; a plain `canceled`
                // row after its TTL cancel went is the TTL's (EXPIRED) — a
                // venue-specific end (margin, self-trade, the dead man's
                // schedule …) keeps its reason. An order seen resting after
                // its TTL cancel went may have landed late (`TtlTable::seen`).
                let mut reason = u.reason;
                if u.kind == EV_RESTING {
                    self.ttl.seen(slot, client_oid);
                } else if self.ttl.forget(slot, client_oid) == Some(true)
                    && u.kind == EV_CANCELED
                    && u.reason == ORDER_EVENT_REASON_CANCEL_REQUESTED
                {
                    reason = ORDER_EVENT_REASON_EXPIRED;
                }
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
                    reason,
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
    slots: SlotMask,
    sk: secp256k1::SecretKey,
    network: Network,
    nonce: NonceSource,
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
    /// XH4e: the last minute's IP weight and WebSocket messages, and this
    /// gateway's shares of each.
    ip: RateWindow,
    ws: RateWindow,
    ip_cap: u64,
    ws_cap: u64,
    /// Frames both connections had written at the last charge.
    frames: u64,
}

// The gateway's inline state — what `new` returns and its thread takes (its
// tables are on the heap): 7 936 B released at XH4e, 8 000 B with debug
// assertions (mio 0.8's debug-only selector ids in its `Poll`s and
// `TcpStream`s), a few words more since XH4f's two boxed Router tables and
// two counters, one more since XH4h's `NonceSource`, one more since XH5b-1's
// spot table, one more since XH4i-1's outcome flag in the coin map — held
// under 8 KiB: a
// bound, not a pin, so a rustls or mio
// bump does not break the build while the struct stays small.
const _: () = assert!(core::mem::size_of::<HlGateway>() <= 8_192);

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
        if cfg.ip_weight_per_min == 0 || cfg.ws_msgs_per_min == 0 {
            return Err(GatewayErr::Rates);
        }
        let addr = resolve(&cfg.host, cfg.port)?;
        let server_name = TlsTransport::server_name_from_host(&cfg.host)
            .map_err(|_| GatewayErr::BadServerName)?;
        let http =
            HlHttp::new(&cfg.host, cfg.port, Arc::clone(&cfg.tls)).map_err(|_| GatewayErr::Http)?;
        let poll = Poll::new().map_err(|_| GatewayErr::Poll)?;
        let GatewayEnd { cmd, evt, stats } = end;
        // COPY: the ≤ 1.4 KiB coin table moves into place once at boot,
        // and the gateway itself (≤ 8 KiB, const-asserted; its tables on
        // the heap) moves a few times at boot — out of this `Result`, into
        // its thread's closure and the spawn — rejected: boxing each,
        // which buys nothing at boot.
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
            ip: RateWindow::new(),
            ws: RateWindow::new(),
            ip_cap: u64::from(cfg.ip_weight_per_min),
            ws_cap: u64::from(cfg.ws_msgs_per_min),
            frames: 0,
            host: cfg.host,
            port: cfg.port,
            addr,
            server_name,
            tls: cfg.tls,
            slots: cfg.slots,
            sk,
            network: cfg.network,
            nonce: NonceSource::own(),
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

    /// **XMM XH4h S3.** Sign with nonces from `stream`, the agent
    /// address's one stream in this process, so this gateway never draws a
    /// nonce another signer of the same agent drew (a blocking arm on the
    /// engine thread). Boot: before the gateway's thread starts — no post
    /// is signed before its first turn.
    pub fn share_nonce(&mut self, stream: Arc<SharedNonce>) {
        self.nonce.share(stream);
    }

    /// Whether this gateway draws its nonces from a shared stream (S3).
    #[must_use]
    pub const fn nonce_shared(&self) -> bool {
        self.nonce.is_shared()
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
        self.charge_frames(now);
        worked |= self.pump_commands(now);
        worked |= self.sweep(now);
        worked |= self.expire(now);
        self.keepalive(now);
        self.settle(now);
        self.rt
            .stats
            .set_gauges(self.rt.pending.live() as u64, self.mbb.active().is_some());
        if now.saturating_sub(self.beat_ns) >= BEAT_NS {
            // XH4f (D9): the TTLs that ended since the last beat.
            self.fire_ttls(now);
            // XH4f: the rate windows move with the clock, not only with
            // spend (XH4e moved them on spend alone): a quiet second leaves
            // them before the congestion word and the throttle read them.
            self.ip.advance(now);
            self.ws.advance(now);
            // XH4e: the congestion word with the heartbeat — at most ten
            // stores a second on the line the engine polls; it is advisory
            // (`admit` judges the throttle itself, command by command).
            self.publish_congestion(now);
            // XH4f: the recent answers' worst round trip, with it.
            self.rt.stats.set_rtt_worst(self.rt.rtt.worst(now));
            // XH4f (D11): the counters and the windows, for `/metrics`.
            self.publish_counters();
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

    /// HK4, TEST-ONLY (`testvenue`, never in a shipping build): the active
    /// socket dies under its next write, after the frame left — for a post,
    /// the re-send branch (the SAME signed bytes by REST).
    #[cfg(feature = "testvenue")]
    pub fn kill_ws_after_next_send(&mut self) {
        if let Some(a) = self.mbb.active() {
            self.conns[usize::from(a)].die_after_next_send();
        }
    }

    /// Whether a connection is between its dial and Ready (TLS, upgrade,
    /// subscribing) — gate 114 judges only windows with none (HK4).
    #[must_use]
    pub fn dialing(&self) -> bool {
        let mut s = 0usize;
        while s < 2 {
            let p = self.conns[s].phase();
            if p != Phase::Down && p != Phase::Ready {
                return true;
            }
            s += 1;
        }
        false
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

    // ---- rates (XH4e) ------------------------------------------------

    /// Charge the message window with every frame both connections wrote
    /// since the last charge (posts, pings, pongs, subscribes) and the IP
    /// window with the weight answers added; move both to `now`.
    fn charge_frames(&mut self, now: u64) {
        let f = self.conns[0].frames() + self.conns[1].frames();
        self.ws.add(
            now,
            u32::try_from(f.saturating_sub(self.frames)).unwrap_or(u32::MAX),
        );
        self.frames = f;
        let w = self.rt.read_weight;
        self.rt.read_weight = 0;
        self.ip.add(now, w);
    }

    /// Charge the IP window with post `id`'s weight: an action 1 + ⌊n/40⌋
    /// (n its items), a read its REST weight.
    fn charge_ip(&mut self, id: u64, now: u64) {
        let w = match self.rt.pending.get(id) {
            Some(rec) if usize::from(rec.class) == CLASS_READ => read_ip_weight(rec.items[0].kind),
            Some(rec) => 1 + u32::from(rec.n) / 40,
            None => 1,
        };
        self.ip.add(now, w);
    }

    /// Either window at [`THROTTLE_PCT`] of its share.
    fn throttled(&self) -> bool {
        self.ip.at_least(self.ip_cap, THROTTLE_PCT) || self.ws.at_least(self.ws_cap, THROTTLE_PCT)
    }

    /// The congestion word: the worst of a venue rate-limit answer in the
    /// last 10 s, growth throttled — or no socket Ready (XMM XH5c-3: every
    /// post would go by REST, out of the RTT word; the member holds) — and
    /// busy.
    fn publish_congestion(&mut self, now: u64) {
        let limited = self.rt.rate_limited_ns != 0
            && now.saturating_sub(self.rt.rate_limited_ns) < RATE_LIMITED_NS;
        let level = if limited {
            CONGESTION_RATE_LIMITED
        } else if self.throttled() || self.ready_conn().is_none() {
            CONGESTION_THROTTLED
        } else if self.ip.at_least(self.ip_cap, BUSY_PCT)
            || self.ws.at_least(self.ws_cap, BUSY_PCT)
            || self.rt.pending.live() >= BUSY_IN_FLIGHT
            || self.rt.rtt_ewma_ns >= BUSY_RTT_NS
        {
            CONGESTION_BUSY
        } else {
            CONGESTION_NONE
        };
        self.rt.stats.set_congestion(level);
    }

    /// XH4f (D11): publish the gateway's own counters and its rate windows'
    /// sums for the arm's report ([`pw`]), each stored on change.
    fn publish_counters(&self) {
        let c = &self.rt.counters;
        let s = &self.rt.stats;
        s.publish(pw::FILLS, c.fills);
        s.publish(pw::FILLS_SCAN_FAILED, c.fills_scan_failed);
        s.publish(pw::UPDATES, c.updates);
        s.publish(pw::UPDATES_IGNORED, c.updates_ignored);
        s.publish(pw::UPDATES_SCAN_FAILED, c.updates_scan_failed);
        s.publish(pw::ANSWERS_UNREADABLE, c.answers_unreadable);
        s.publish(pw::ANSWERS_MISMATCHED, c.answers_mismatched);
        s.publish(pw::WS_ERRORS, c.ws_errors);
        s.publish(pw::LOCAL_REFUSALS, c.local_refusals);
        s.publish(pw::CANCELS_UNTIMED, c.cancels_untimed);
        s.publish(pw::DIALS, c.dials);
        s.publish(pw::CONN_DROPS, c.conn_drops);
        s.publish(pw::SWEEP_ROUNDS, c.sweep_rounds);
        s.publish(pw::SWEEP_UNMAPPED, c.sweep_unmapped);
        s.publish(pw::THROTTLED, c.throttled);
        s.publish(pw::TTL_CANCELS, c.ttl_cancels);
        s.publish(pw::TTL_FULL, c.ttl_full);
        s.publish(pw::RATE_IP, self.ip.sum());
        s.publish(pw::RATE_WS, self.ws.sum());
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
        let refusal = if !owns_slot(self.slots, c.slot) {
            Some(REASON_LOCAL)
        } else if growth && self.rt.stats.halt_reason() != HALT_NONE {
            Some(REASON_HALTED)
        } else if growth && !self.rt.snapshot_absorbed {
            Some(REASON_NOT_READY)
        } else if growth && self.throttled() {
            // XH4e: the rates keep their room for cancels and reads.
            self.rt.counters.throttled += 1;
            Some(REASON_THROTTLED)
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
        // XMM XH5d: an action carries one grouping — a place bidding
        // another ALO priority than the open ALO batch's posts every open
        // batch first, as a full one does.
        let p = if k == CLASS_ALO { c.arg } else { 0 };
        if self.batches.head[k].n != 0 && self.batches.head[k].p != p {
            self.flush_open(now);
        }
        if self.batches.head[k].n == 0 && !self.open(k, now) {
            self.refuse(c, REASON_LOCAL, now);
            return;
        }
        let b = &mut *self.batches;
        let h = &mut b.head[k];
        let i = h.n;
        h.p = p;
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
        // XH4f (D9): a place's TTL runs from its admission; a modify's
        // replacement inherits its original's. The arm's own cancel ends it
        // only once it succeeds (`on_answer`): one that fails leaves the TTL
        // to cancel the order.
        let kept = match c.kind {
            CMD_PLACE if c.time_ms != 0 => {
                self.rt
                    .ttl
                    .track(c.slot, c.client_oid, c.asset, now, c.time_ms)
            }
            CMD_MODIFY => self
                .rt
                .ttl
                .inherit(c.slot, c.prev_client_oid, c.client_oid, c.asset),
            _ => true,
        };
        self.rt.counters.ttl_full += u64::from(!kept);
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
                arg: c.arg,
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
            if c.kind == CMD_READ_DAY {
                // The day read's start rides where a modify's target would,
                // its end where an order's push stamp would.
                rec.items[0].prev = c.time_ms;
                rec.items[0].ts_ns = c.prev_client_oid;
            }
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
        if c.kind == CMD_CANCEL && c.prev_client_oid == TTL_CANCEL_MARK {
            // XH4f (D9): the TTL's own cancel: again at the next beat; the
            // arm hears nothing of it.
            self.rt.ttl.unsend(c.slot, c.client_oid);
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
            p: 0,
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
        // The one routing decision (XMM HD2): a post with no socket Ready
        // goes by REST (a read is refused), sealed straight into the REST
        // client's window — and posted before anything else writes it.
        let conn = self.ready_conn();
        let rest = k != CLASS_READ && conn.is_none();
        match self.seal(k, &h, rest) {
            Some(sealed) => self.send(k, h.id, sealed, conn, now),
            None => {
                self.rt.counters.local_refusals += 1;
                self.rt.fail_all(h.id, EV_REJECTED, REASON_LOCAL, now);
            }
        }
    }

    /// Encode, sign and render the post into the TX buffer — or, `rest`
    /// (no socket Ready), the envelope alone into the REST client's
    /// `/exchange` window, where it will go from (XMM HD2). `None` = the
    /// clock, the signer or a buffer refused: nothing was sent.
    fn seal(&mut self, k: usize, h: &BatchHead, rest: bool) -> Option<Sealed> {
        if k == CLASS_READ {
            return self.seal_read(h);
        }
        let n = h.n;
        let b = &*self.batches;
        let mp_n = match k {
            CLASS_ALO => encode_order_grouped(&mut self.mp, &b.alo[..n], grouping(h.p)),
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
        let (dst, pfx) = if rest {
            (self.http.body_mut(), 0)
        } else {
            let tx = &mut self.tx[ROOM..];
            let pfx = ws_post_open(tx, h.id).ok()?;
            (tx, pfx)
        };
        let head = envelope_open(&mut dst[pfx..]).ok()?;
        let at = pfx + head;
        let aj = match k {
            CLASS_ALO => order_json_grouped(&mut dst[at..], &b.alo[..n], grouping(h.p)),
            CLASS_TAKER => order_json(&mut dst[at..], &b.taker[..n], GROUPING),
            CLASS_MODIFY => batch_modify_json(&mut dst[at..], &b.modify[..n]),
            CLASS_CANCEL => cancel_by_cloid_json(&mut dst[at..], &b.cancel[..n]),
            _ => encode_control(&b.control, &mut dst[at..], true),
        }
        .ok()?;
        let env_len =
            envelope_close(&mut dst[pfx..], head + aj, nonce, &sig, None, expires).ok()?;
        if rest {
            return Some(Sealed {
                env: 0,
                env_len,
                ws_len: 0,
                rest,
            });
        }
        let ws_len = ws_post_close(dst, pfx + env_len).ok()?;
        Some(Sealed {
            env: ROOM + pfx,
            env_len,
            ws_len,
            rest,
        })
    }

    /// A read (XH4d): the post head, the `/info` body rendered in place
    /// after it, the tail — unsigned, no envelope, no REST twin.
    fn seal_read(&mut self, h: &BatchHead) -> Option<Sealed> {
        let (kind, start_ms, end_ms) = {
            let item = &self.rt.pending.get(h.id)?.items[0];
            (item.kind, item.prev, item.ts_ns)
        };
        let tx = &mut self.tx[ROOM..];
        let at = ws_info_open(tx, h.id).ok()?;
        let (dst, master) = (&mut tx[at..], &self.master);
        let body = match kind {
            CMD_READ_ORDERS => open_orders_request(dst, master),
            CMD_READ_LIMIT => rate_limit_request(dst, master),
            CMD_READ_DAY => fills_by_time_request(dst, master, start_ms, end_ms),
            CMD_READ_SPOT => spot_state_request(dst, master),
            _ => perp_state_request(dst, master),
        }
        .ok()?;
        let ws_len = ws_post_close(tx, at + body).ok()?;
        Some(Sealed {
            env: ROOM + at,
            env_len: 0,
            ws_len,
            rest: false,
        })
    }

    /// The connection new posts go on: the active one, once Ready. None (a
    /// swap's gap, an outage): a post goes by REST, a read not at all — and
    /// the congestion word reads throttled (XMM XH5c-3).
    #[inline]
    fn ready_conn(&self) -> Option<u8> {
        match self.mbb.active() {
            Some(a) if self.conns[usize::from(a)].phase() == Phase::Ready => Some(a),
            _ => None,
        }
    }

    /// Write a sealed post on the live connection, or by REST — a post
    /// sealed for REST always goes there; a read only on the live
    /// connection: with none it is refused, unsent. `conn` is `flush`'s
    /// [`Self::ready_conn`], the one the post was sealed by.
    fn send(&mut self, k: usize, id: u64, s: Sealed, conn: Option<u8>, now: u64) {
        let active = match conn {
            Some(a) if !s.rest => a,
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
        self.charge_ip(id, now);
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

    /// The REST fallback: the envelope from the REST client's `/exchange`
    /// window — rendered there at seal time when no socket was Ready
    /// (XMM HD2), copied there out of the TX buffer for a re-send.
    fn post_rest(&mut self, id: u64, s: Sealed, resend: bool) {
        self.rt.stats.note_rest_fallback();
        let written = now_ns();
        self.charge_ip(id, written);
        if let Some(rec) = self.rt.pending.get_mut(id) {
            rec.conn = CONN_REST;
            rec.written_ns = written;
        }
        if !s.rest {
            debug_assert!(resend, "only a re-send copies its envelope");
            // COPY: the envelope (≤ TX_CAP = 16 KiB, const-asserted to fit)
            // out of `tx` into the REST window — a re-send only: its socket
            // died under the write, and the SAME signed bytes live only in
            // `tx`. Rejected: a second write from `tx` (a second record) or a
            // gather write (`write_vectored` collects a `Vec` per call) — one
            // more allocation either way; the HTTP head rendered before the
            // envelope in `tx` (≈ 133 B per re-send; core-net's raw form public).
            self.http.body_mut()[..s.env_len].copy_from_slice(&self.tx[s.env..s.env + s.env_len]);
        }
        let res = self.http.post(s.env_len);
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
        let mut sent = 0u64;
        let mut i = 0usize;
        while i < n {
            let h = self.rt.sweep_hits[i];
            i += 1;
            if !owns_slot(self.slots, h.slot) {
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
            sent += 1;
            self.admit(&HlCmd::cancel(h.slot, h.client_oid, h.asset).at(now), now);
        }
        // XH4e: the round's cancels, which the engine charges to the
        // address — one add a round.
        self.rt.stats.note_sweep_cancels(sent);
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

    /// XH4f (D9): cancel every tracked order whose TTL ended — untimed
    /// (`cancelByCloid`, XG-3), marked so its answers read `EXPIRED`. XG-2
    /// binds them as it binds the sweep: the rest go at the next beat. The
    /// engine charges them to the address with the sweeps' (one add a beat).
    fn fire_ttls(&mut self, now: u64) {
        let mut sent = 0u64;
        self.rt.ttl.rewind();
        while !self.xg2_full() {
            let Some((slot, client_oid, asset)) = self.rt.ttl.next_due(now) else {
                break;
            };
            let mut c = HlCmd::cancel(slot, client_oid, asset).at(now);
            c.prev_client_oid = TTL_CANCEL_MARK;
            self.admit(&c, now);
            // Admitted, not refused unsent (`refuse` re-arms that): out.
            self.rt.ttl.went(slot, client_oid);
            sent += 1;
        }
        if sent > 0 {
            self.rt.counters.ttl_cancels += sent;
            self.rt.stats.note_sweep_cancels(sent);
            self.flush_open(now);
        }
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
    /// XH4f (D9, alloc r4): a TTL cancel's refusal uses up a try unless it
    /// never left this host or met the rate limit — those six of the 256
    /// reason bytes, no other.
    #[test]
    fn a_ttl_try_counts_but_for_the_five_never_left_reasons_and_the_rate_limit() {
        let free = [
            super::REASON_LOCAL,
            super::REASON_HALTED,
            super::REASON_NOT_READY,
            super::REASON_THROTTLED,
            super::REASON_TRANSPORT,
            super::REASON_RATE_LIMIT,
        ];
        let mut i = 0;
        while i < free.len() {
            assert!(!super::ttl_try_counts(free[i]), "reason {}", free[i]);
            i += 1;
        }
        let mut n = 0u32;
        let mut r = 0u16;
        while r <= u16::from(u8::MAX) {
            n += u32::from(!super::ttl_try_counts(r as u8));
            r += 1;
        }
        assert_eq!(n, 6, "those six of the 256 reason bytes, no other");
    }

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

    /// XH4f: the reads' steady weight — XH4d's 5 s periods alone were 71 %
    /// of the share (busy for good); 10 s leaves them at 38 %. XH5b-1: the
    /// spot sheet adds 2 a period — 164 at 10 s (41 %), 68 at XH5c's 30 s.
    #[test]
    fn the_periodic_reads_weigh_what_the_amendment_says() {
        assert_eq!(read_weight_per_min(5_000, 60_000, false), 284);
        assert_eq!(read_weight_per_min(10_000, 60_000, false), 152);
        assert_eq!(read_weight_per_min(10_000, 60_000, true), 164);
        assert_eq!(read_weight_per_min(30_000, 60_000, true), 68);
        assert_eq!(read_weight_per_min(0, 60_000, true), u64::MAX);
        assert_eq!(
            (
                read_ip_weight(CMD_READ_STATE),
                read_ip_weight(CMD_READ_SPOT),
                read_ip_weight(CMD_READ_ORDERS),
                read_ip_weight(CMD_READ_LIMIT),
                read_ip_weight(CMD_READ_DAY)
            ),
            (2, 2, 20, 20, 20)
        );
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
        odd.kind = CMD_RESERVE + 1;
        assert_eq!(class_of(&odd), None, "past the last kind (XMM XH5c)");
        assert_eq!(class_of(&HlCmd::read_limit(6, 1)), Some(CLASS_READ));
        assert_eq!(class_of(&HlCmd::read_day(6, 1, 0, 0)), Some(CLASS_READ));
        assert_eq!(class_of(&HlCmd::read_spot(6, 1)), Some(CLASS_READ));
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

    /// XH4i-1: an outcome map names a leg by its own `#<enc>` coin —
    /// canonical digits whose id fits — refuses to bind a `#` name, and
    /// still binds perps by name; a perps-only map (slot 6's) leaves every
    /// `#` coin unknown.
    #[test]
    fn an_outcome_map_names_legs_by_their_own_coins() {
        use crate::asset::ASSET_BASE;
        let mut m = CoinMap::with_outcomes();
        assert!(m.insert(b"ETH", 1));
        assert!(!m.insert(b"#1941", 7), "a leg is never bound by name");
        assert_eq!(m.asset(b"ETH"), 1);
        assert_eq!(m.asset(b"#1941"), ASSET_BASE + 1941);
        assert_eq!(m.asset(b"#0"), ASSET_BASE);
        // `#4194967295` reads as `u32::MAX` — the sentinel itself: an id
        // no roll can bind, unmapped and never cancelled, on purpose.
        let bad: [&[u8]; 8] = [
            b"#",
            b"#01941",
            b"#19a1",
            b"#-1",
            b"+1941",
            b"#4194967295",
            b"#4194967296",
            b"#42949672950",
        ];
        let mut i = 0usize;
        while i < bad.len() {
            assert_eq!(m.asset(bad[i]), ASSET_UNKNOWN, "{:?}", bad[i]);
            i += 1;
        }
        assert_eq!(
            CoinMap::new().asset(b"#1941"),
            ASSET_UNKNOWN,
            "slot 6's map, as before"
        );
    }

    /// **S16-4 — a slot-11 gateway posts for slot 11 and refuses BIN15's
    /// slot 3**; a slot-3 gateway is as before. A `u8` mask's `1 << 11`
    /// read as slot 3 in a release build.
    #[test]
    fn a_slot_11_gateway_owns_slot_11_and_never_slot_3() {
        let tls = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        );
        let at = |slot: u8| {
            GatewayCfg::production(
                "localhost".to_string(),
                Arc::clone(&tls),
                Network::Testnet,
                [0xA5; 20],
                slot,
            )
            .slots
        };
        let eleven = at(11);
        assert_eq!(eleven, 1 << 11);
        assert!(owns_slot(eleven, 11));
        assert!(!owns_slot(eleven, 3), "BIN15's slot is not slot 11's");
        let three = at(3);
        assert_eq!(three, 1 << 3, "slot 3's mask unchanged");
        assert!(owns_slot(three, 3));
        assert!(!owns_slot(three, 11));
        assert_eq!(at(15), 1 << 15);
        assert!(owns_slot(at(15), 15));
        // Past the slot space, whatever the mask says.
        let mut b = 16u16;
        while b <= u16::from(u8::MAX) {
            assert!(!owns_slot(SlotMask::MAX, b as u8), "byte {b}");
            b += 1;
        }
        assert!(!owns_slot(SlotMask::MAX, SWEEP_SLOT));
        assert!(!owns_slot(SlotMask::MAX, SLOT_FOREIGN));
        // Every slot's gateway owns its own slot and no other.
        let mut s = 0u8;
        while usize::from(s) < EXEC_SLOTS {
            let m = at(s);
            let mut t = 0u8;
            while usize::from(t) < EXEC_SLOTS {
                assert_eq!(owns_slot(m, t), s == t, "gateway {s}, slot {t}");
                t += 1;
            }
            s += 1;
        }
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

    /// A router with one read of `c`'s kind in flight, as post 5.
    fn router_with_read(c: &HlCmd) -> (Router, HlEngineEnd) {
        router_with_read_on(c, CoinMap::new())
    }

    /// XH4i: a router over `coins` with read `c` in flight.
    fn router_with_read_on(c: &HlCmd, coins: CoinMap) -> (Router, HlEngineEnd) {
        let (eng, gw) = hl_duplex();
        let mut rt = Router::new(gw.evt, gw.stats, coins);
        let rec = rt.pending.claim(5, 1).expect("claim");
        rec.class = CLASS_READ as u8;
        rec.conn = CONN_REST;
        rec.timer = None;
        rec.written_ns = 1;
        rec.n = 1;
        rec.items[0] = ItemRef::of(c);
        rt.inflight_items = 1;
        (rt, eng)
    }

    /// XH4e: the budget read's answer is one end carrying the venue's
    /// figures; the day read's is one end carrying its slot's sum, and its
    /// rows add IP weight (1 per 20); a read of either that does not scan
    /// fails with its own code.
    #[test]
    fn the_budget_and_day_reads_answer_one_end_each() {
        let limit = br#"{"type":"userRateLimit","data":{"cumVlm":"1500.5","nRequestsUsed":2890,"nRequestsCap":11500,"nRequestsSurplus":7}}"#;
        let (mut rt, mut eng) = router_with_read(&HlCmd::read_limit(6, 1));
        rt.on_read_answer(5, limit, 9);
        let ev = drained(&mut eng);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            (ev[0].kind, ev[0].reason, ev[0].client_oid, ev[0].aux),
            (EV_READ_DONE, READ_LIMIT, 1, 2_890)
        );
        assert_eq!((ev[0].px_1e8, ev[0].sz_1e8), (150_050_000_000, 7));

        let mut hex = [0u8; 34];
        let n = crate::cloid::to_hex(&crate::cloid::encode(6, 7), &mut hex);
        let cloid = String::from_utf8(hex[..n].to_vec()).unwrap();
        let mut rows = Vec::new();
        let mut t = 0u64;
        while t < 40 {
            rows.push(format!(
                r#"{{"coin":"ETH","px":"10.0","sz":"1.0","side":"B","time":{},"tid":{t},"oid":{t},"cloid":"{cloid}"}}"#,
                1_000 + t
            ));
            t += 1;
        }
        let day = format!(
            r#"{{"type":"userFillsByTime","data":[{}]}}"#,
            rows.join(",")
        );
        let (mut rt, mut eng) = router_with_read(&HlCmd::read_day(6, 2, 1_000, 2_000));
        rt.on_read_answer(5, day.as_bytes(), 9);
        let ev = drained(&mut eng);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            (
                ev[0].kind,
                ev[0].reason,
                ev[0].aux,
                ev[0].px_1e8,
                ev[0].sz_1e8,
                ev[0].venue_ms
            ),
            (EV_READ_DONE, READ_DAY, 40, 40_000_000_000, 0, 1_039)
        );
        assert_eq!(rt.read_weight, 2, "1 per 20 rows");

        let (mut rt, mut eng) = router_with_read(&HlCmd::read_day(6, 3, 1_000, 2_000));
        rt.on_read_answer(5, br#"{"type":"userFillsByTime","data":{}}"#, 9);
        let ev = drained(&mut eng);
        assert_eq!((ev[0].kind, ev[0].reason), (EV_READ_FAILED, READ_DAY));
        let (mut rt, mut eng) = router_with_read(&HlCmd::read_limit(6, 4));
        rt.on_read_answer(5, br#"{"nRequestsUsed":1}"#, 9);
        let ev = drained(&mut eng);
        assert_eq!((ev[0].kind, ev[0].reason), (EV_READ_FAILED, READ_LIMIT));
        assert_eq!(read_code(CMD_READ_ORDERS), READ_ORDERS);
        assert_eq!(read_code(CMD_READ_STATE), READ_STATE);
    }

    /// XH5b-1: the spot sheet's answer is one end carrying the USDC
    /// `total` — on the venue's own testnet body, $999 over fourteen rows,
    /// USDC listed; a sheet that lists no USDC says so; one that does not
    /// scan fails with its own code.
    #[test]
    fn the_spot_read_answers_one_end_with_the_usdc_total() {
        let body = format!(
            r#"{{"type":"spotClearinghouseState","data":{}}}"#,
            include_str!("../tests/fixtures/hl/spot_state_testnet.json").trim()
        );
        let (mut rt, mut eng) = router_with_read(&HlCmd::read_spot(6, 1));
        rt.on_read_answer(5, body.as_bytes(), 9);
        let ev = drained(&mut eng);
        assert_eq!(ev.len(), 1, "no rows: one end");
        assert_eq!(
            (
                ev[0].kind,
                ev[0].reason,
                ev[0].client_oid,
                ev[0].aux,
                ev[0].px_1e8
            ),
            (EV_READ_DONE, READ_SPOT, 1, 14, 99_900_000_000)
        );
        assert_eq!(ev[0].sz_1e8, 1, "USDC listed");
        let (mut rt, mut eng) = router_with_read(&HlCmd::read_spot(6, 2));
        rt.on_read_answer(
            5,
            br#"{"type":"spotClearinghouseState","data":{"balances":[]}}"#,
            9,
        );
        let ev = drained(&mut eng);
        assert_eq!(
            (ev[0].kind, ev[0].reason, ev[0].px_1e8, ev[0].sz_1e8),
            (EV_READ_DONE, READ_SPOT, 0, 0),
            "no USDC listed: said so, for the arm to refuse"
        );
        let (mut rt, mut eng) = router_with_read(&HlCmd::read_spot(6, 3));
        rt.on_read_answer(5, br#"{"type":"spotClearinghouseState","data":{}}"#, 9);
        let ev = drained(&mut eng);
        assert_eq!((ev[0].kind, ev[0].reason), (EV_READ_FAILED, READ_SPOT));
        assert_eq!(read_code(CMD_READ_SPOT), READ_SPOT);
    }

    /// XH4i-2: on an outcome map the spot read reports each leg the
    /// account holds — one `EV_BALANCE` per `+<enc>` row with a holding,
    /// its values raw (a negative total or basis included, a missing
    /// basis 0; a name that does not parse is `ASSET_UNKNOWN`, which an
    /// outcome arm counts unseen), never a settled husk or another token —
    /// then its end, which counts them in `oid` and carries the mark of a
    /// sheet read for legs; on a perps-only map (slot 6's) the same sheet
    /// answers one unmarked end, as before.
    #[test]
    fn on_an_outcome_map_the_spot_read_reports_the_held_legs() {
        use crate::asset::ASSET_BASE;
        let body = br#"{"type":"spotClearinghouseState","data":{"balances":[{"coin":"USDC","token":0,"total":"999.0","hold":"0.0","entryNtl":"0.0"},{"coin":"HYPE","token":150,"total":"1.5","hold":"0.0","entryNtl":"40.0"},{"coin":"+194180","token":2,"total":"25.0","hold":"5.0","entryNtl":"10.25"},{"coin":"+194181","token":3,"total":"0.0","hold":"0.0"},{"coin":"+0195","token":4,"total":"3.0","hold":"0.0","entryNtl":"1.2"},{"coin":"+194190","token":5,"total":"-2.0","hold":"0.0","entryNtl":"-1.5"},{"coin":"+194200","token":6,"total":"4.0","hold":"0.0"}]}}"#;
        let mut k = 0usize;
        while k < 2 {
            let outcomes = k == 0;
            k += 1;
            let coins = if outcomes {
                CoinMap::with_outcomes()
            } else {
                CoinMap::new()
            };
            let (mut rt, mut eng) = router_with_read_on(&HlCmd::read_spot(3, 1), coins);
            rt.on_read_answer(5, body, 9);
            let ev = drained(&mut eng);
            let end = ev.last().expect("an end");
            assert_eq!(
                (
                    end.kind,
                    end.reason,
                    end.client_oid,
                    end.aux,
                    end.px_1e8,
                    end.sz_1e8
                ),
                (EV_READ_DONE, READ_SPOT, 1, 7, 99_900_000_000, 1)
            );
            if !outcomes {
                assert_eq!(
                    (ev.len(), end.oid, end.asset),
                    (1, 0, 0),
                    "slot 6's map: one unmarked end, as before"
                );
                continue;
            }
            assert_eq!(
                (ev.len(), end.oid, end.asset),
                (5, 4, SPOT_LEGS_READ),
                "four held legs, then the marked end"
            );
            assert_eq!(
                (
                    ev[0].kind,
                    ev[0].asset,
                    ev[0].sz_1e8,
                    ev[0].px_1e8,
                    ev[0].slot,
                    ev[0].client_oid
                ),
                (
                    EV_BALANCE,
                    ASSET_BASE + 194_180,
                    2_500_000_000,
                    1_025_000_000,
                    3,
                    1
                )
            );
            assert_eq!(
                (ev[1].kind, ev[1].asset, ev[1].sz_1e8, ev[1].px_1e8),
                (EV_BALANCE, ASSET_UNKNOWN, 300_000_000, 120_000_000),
                "a leg whose name does not parse is still reported: the arm counts it unseen"
            );
            assert_eq!(
                (ev[2].asset, ev[2].sz_1e8, ev[2].px_1e8),
                (ASSET_BASE + 194_190, -200_000_000, -150_000_000),
                "values go out raw: `recon::leg_cost_1e6` values them"
            );
            assert_eq!(
                (ev[3].asset, ev[3].sz_1e8, ev[3].px_1e8),
                (ASSET_BASE + 194_200, 400_000_000, 0),
                "no basis reads 0"
            );
        }
        // Legs on a sheet that lists no USDC: the rows, then a marked end
        // that says no worth — for the arm to discard whole.
        let (mut rt, mut eng) =
            router_with_read_on(&HlCmd::read_spot(3, 2), CoinMap::with_outcomes());
        rt.on_read_answer(
            5,
            br#"{"type":"spotClearinghouseState","data":{"balances":[{"coin":"+194180","token":2,"total":"25.0","hold":"0.0","entryNtl":"10.25"}]}}"#,
            9,
        );
        let ev = drained(&mut eng);
        assert_eq!(
            (
                ev.len(),
                ev[0].kind,
                ev[1].kind,
                ev[1].sz_1e8,
                ev[1].px_1e8,
                ev[1].oid
            ),
            (2, EV_BALANCE, EV_READ_DONE, 0, 0, 1)
        );
        // A full sheet — 256 held legs — goes out whole: every row, then the
        // end (XG-2's room, const-asserted).
        let mut rows = Vec::new();
        let mut j = 0u32;
        while j < 256 {
            rows.push(format!(
                r#"{{"coin":"+{}","token":{j},"total":"1.0","hold":"0.0","entryNtl":"0.5"}}"#,
                10 * j + 1
            ));
            j += 1;
        }
        let full = format!(
            r#"{{"type":"spotClearinghouseState","data":{{"balances":[{}]}}}}"#,
            rows.join(",")
        );
        let (mut rt, mut eng) =
            router_with_read_on(&HlCmd::read_spot(3, 3), CoinMap::with_outcomes());
        rt.on_read_answer(5, full.as_bytes(), 9);
        let ev = drained(&mut eng);
        assert_eq!(ev.len(), 257);
        assert_eq!(
            (
                ev[255].kind,
                ev[255].asset,
                ev[256].kind,
                ev[256].oid,
                ev[256].aux
            ),
            (EV_BALANCE, ASSET_BASE + 2_551, EV_READ_DONE, 256, 256)
        );
    }

    /// XMM XH5c-3: a REST post's round trip goes to its own series alone —
    /// never the socket's histograms, the congestion average or the RTT
    /// word's window; the same post answered on a socket feeds them all.
    #[test]
    fn a_rest_round_trip_has_its_own_series_out_of_the_rtt_word() {
        let at = 2_000_000_001u64;
        let (mut rt, _eng, id) = router_with_post(CLASS_ALO, 1);
        rt.finish(id, at);
        assert_eq!(
            (rt.stats.rest_rtt.count(), rt.stats.rest_rtt.max_ns()),
            (1, 2_000_000_000)
        );
        assert_eq!(
            (rt.stats.post_rtt.count(), rt.rtt.worst(at), rt.rtt_ewma_ns),
            (0, 0, 0)
        );
        let (mut rt, _eng, id) = router_with_post(CLASS_ALO, 1);
        rt.pending.get_mut(id).expect("in flight").conn = 0;
        rt.finish(id, at);
        assert_eq!(
            (
                rt.stats.post_rtt.count(),
                rt.rtt.worst(at),
                rt.stats.rest_rtt.count()
            ),
            (1, 2_000_000_000, 0)
        );
        assert!(rt.rtt_ewma_ns > 0);
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

    /// XH4i-3b-2b: an outcome map's first snapshot hands the arm its legs'
    /// recent rows — flagged, with no fee, before the snapshot's end — and
    /// keeps an older leg row and a perp's (no leg) as history; a later
    /// snapshot re-delivers nothing.
    #[test]
    fn on_an_outcome_map_the_first_snapshot_hands_the_legs_history_to_the_arm() {
        let (eng_end, gw) = hl_duplex();
        let mut eng = eng_end;
        let mut rt = Router::new(gw.evt, gw.stats, CoinMap::with_outcomes());
        let snap = br##"{"channel":"userFills","data":{"isSnapshot":true,"user":"0x","fills":[{"coin":"#1941","px":"0.3","sz":"4","side":"B","time":199000000,"oid":1,"tid":5},{"coin":"#1941","px":"0.3","sz":"1","side":"A","time":100000000,"oid":2,"tid":6},{"coin":"ETH","px":"1","sz":"1","side":"B","time":200000000,"oid":3,"tid":7}]}}"##;
        rt.on_frame(snap, 1);
        let e = drained(&mut eng);
        assert_eq!(e.len(), 2, "the leg's recent row, then the snapshot's end");
        assert_eq!(
            (e[0].kind, e[0].asset, e[0].sz_1e8, e[0].aux),
            (EV_FILL, crate::asset::ASSET_BASE + 1941, 400_000_000, 5)
        );
        assert_ne!(e[0].flags & EVF_SNAPSHOT, 0, "flagged a snapshot's");
        assert_eq!((e[1].kind, e[1].aux), (EV_SNAPSHOT, 3));
        rt.on_frame(snap, 2);
        assert!(
            drained(&mut eng).is_empty(),
            "a later snapshot re-delivers nothing"
        );
    }

    #[test]
    fn control_commands_are_their_own_class_and_only_leverage_grows() {
        let lev = HlCmd::leverage(6, 1, 0, true, 3);
        let sched = HlCmd::schedule_cancel(6, 2, 1_789_000_005_000);
        assert_eq!(class_of(&lev), Some(CLASS_CONTROL));
        assert_eq!(class_of(&sched), Some(CLASS_CONTROL));
        assert_eq!(class_of(&HlCmd::noop(6, 3)), Some(CLASS_CONTROL));
        let reserve = HlCmd::reserve(6, 4, 10_000);
        assert_eq!(class_of(&reserve), Some(CLASS_CONTROL));
        assert!(
            grows(CMD_RESERVE) && growing(CLASS_CONTROL, CMD_RESERVE),
            "a purchase grows"
        );
        assert_eq!(cmd_bits(CMD_RESERVE), 0, "a control carries no order verb");
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
            arg: 3,
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
        // XMM XH5c: the weight purchase — its msgpack is the SDK vector's
        // encoder (`reserve_weight`), its JSON twin names the same action.
        let reserve = ControlWire {
            kind: CMD_RESERVE,
            arg: 5_000,
            ..ControlWire::default()
        };
        let n = encode_control(&reserve, &mut buf, true).unwrap();
        assert_eq!(
            &buf[..n],
            br#"{"type":"reserveRequestWeight","weight":5000}"#
        );
        let n = encode_control(&reserve, &mut buf, false).unwrap();
        let mut want = [0u8; 64];
        let m = encode_reserve_weight(&mut want, 5_000).unwrap();
        assert_eq!(&buf[..n], &want[..m], "the vector's bytes");
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
