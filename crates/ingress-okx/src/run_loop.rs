// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-okx run-loop
//!
//! OKX v5 public WS over TLS. Same opening-handshake shape as every
//! ingress; steady state is push-driven (no request polling):
//!
//! ```text
//! Connecting ──► NeedsWsWrite ──► AwaitingWsUpgrade ──► Steady ──┐
//!      ▲                                                          │
//!      └──────────────── Closed / Err ────────────────────────────┘
//! ```
//!
//! On entry to `Steady` the driver queues **one** batched
//! `{"op":"subscribe","args":[...]}` op covering every configured
//! `(channel × instrument)` pair — OKX budgets 480 sub/unsub ops per
//! hour, so batching is mandatory (§4.1). Acks land in a
//! `core_net::SubTable`.
//!
//! ## Venue-error policy (WS2 — outage 2026-08-27 §5.2)
//!
//! Venue `error` events are **fatal only until this process has ever
//! received a subscribe ack** from the venue (fail-fast at BOOT: a
//! rejected first-ever subscribe means misconfiguration — refuse to
//! run venue-blind). Once any subscribe has ever been acknowledged,
//! an error event is a NON-FATAL PER-ARG DROP: the venue processes
//! batched args individually (§5.4 proved good args keep flowing
//! beside a rejected one), so the instrument the error names — the
//! post-settlement expired-option class — is simply not subscribed
//! this session while everything else lives. Each drop increments
//! `sub_drops_total`, emits one paired `ChannelId::SubDrop` capture
//! event (§6.6 pairing) and a rate-limited stderr WARN. Reconnects
//! re-send the full configured set (the drop is session-scoped); the
//! 0830/1605 slot restarts remain the chain-refresh mechanism.
//!
//! ## Establishment budget (WS2 — outage §5.3)
//!
//! The keepalive idle timeout is gated on `Steady`, so a session
//! wedged in `Connecting`/`AwaitingWsUpgrade` (blackholed SYN under a
//! non-blocking connect) — or `Steady` with ZERO acknowledged
//! subscriptions, kept "active" by pong replies — could previously
//! live forever. [`run`] now tears the session down with
//! [`RunResult::EstablishTimeout`] when no subscription has been
//! acknowledged within the driver's establishment budget
//! (`core_net::ESTABLISH_BUDGET_NS` default).
//!
//! Keepalive is the venue-specific literal `ping` text frame queued
//! by [`run`] when `core_net::Keepalive` says so (25 s interval vs
//! the venue's 30 s idle cutoff); the literal `pong` answer counts as
//! activity like any frame.
//!
//! Integrity per §6.2: `books` chains `seqId`/`prevSeqId` through
//! [`crate::OkxSeqChain`] (gap ⇒ unsubscribe+subscribe resync +
//! `gaps_total`); `trades` monotonic `seqId` through
//! [`crate::TradeSeqMonitor`] (regression ⇒ `gaps_total`).
//!
//! Everything after the handshake is zero-alloc: parsers slice the
//! rx buffer in place; subscribe payloads render into stack scratch;
//! the only copy is the 64-byte `Tick` moved into the ring.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_metrics::{IngressState, IngressStatus};
use core_net::{
    constant_time_eq, expected_accept, queue_masked_text_frame, read_server_handshake,
    sec_websocket_key_from_seed, write_client_handshake, ws_mask_from_counter, ws_read_frame,
    ws_unmask_in_place, ws_write_pong, HandshakeResult, IoBuf, Keepalive, KeepaliveAction, ReqKind,
    Status, SubErr, SubId, SubTable, Transport, WsOpcode, WsReadResult,
};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{
    Capture, ChannelEvent, ChannelId, OptSummary, Price, Qty, Tick, VenueId, SYMBOL_ID_NONE,
};

use crate::{
    classify, extract_error_inst_id, extract_inst_id, parse_bbo, parse_book_header, parse_trade,
    sub_id_of, write_subscribe_batch, write_unsubscribe_batch, ChainOutcome, OkxChannel,
    OkxInstType, OkxMsgKind, OkxSeqChain, OkxSymbolTable, SubArg, TradeSeqMonitor, TradeSeqOutcome,
    OKX_MAX_SYMBOLS, PING_PAYLOAD,
};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Rx buffer: a `books` 400-level snapshot is ~25 KiB; an M2.3
/// `opt-summary` push can carry summaries for an entire family
/// (order-1.5k instruments live ⇒ ~600 KiB in one frame). 4 MiB
/// (boot-time allocation) leaves headroom for a full-family snapshot
/// ON TOP of a bbo backlog — the M2.3 first live smoke reconnect-
/// looped 103× when a concurrent sanitizer build starved the drain
/// loop and the snapshot no longer fit behind the backlog; the
/// margin absorbs exactly that (log entry 2026-08-22).
pub const RX_BUF_SIZE: usize = 4 * 1024 * 1024;

/// Tx buffer: handshake + one batched subscribe op (~46 B per arg ×
/// [`MAX_SUB_ARGS`] = 144 args ≈ 6.7 KiB with a full M2.2 options
/// block) + resync pairs + pings. 16 KiB keeps ≥2× margin
/// (boot-time allocation).
pub const TX_BUF_SIZE: usize = 16 * 1024;

/// Tick-ring capacity. Must equal `engine::TICK_RING_SIZE` — the cli
/// const-asserts the equality when wiring lanes (8a §3.3 pattern).
pub const TICK_RING_CAP: usize = 16_384;

/// Max configured option FAMILIES (`opt-summary` subscribe args —
/// one per configured underlying; core-config caps underlyings at
/// 16).
pub const OPT_FAMILIES_MAX: usize = 16;

/// Upper bound on `(channel × instrument)` subscribe args (M2
/// partition law): up to 5 channels per STATIC instrument
/// ([`crate::OKX_STATIC_MAX`]) + one `bbo-tbt` arg per OPTION row
/// ([`crate::OKX_OPT_MAX`]) + one family-keyed `opt-summary` arg per
/// configured underlying ([`OPT_FAMILIES_MAX`], M2.3) =
/// 80 + 64 + 16 = 160.
pub const MAX_SUB_ARGS: usize = 5 * crate::OKX_STATIC_MAX + crate::OKX_OPT_MAX + OPT_FAMILIES_MAX;

/// Stack scratch for one rendered subscribe batch (~46 B/arg × 144
/// args ≈ 6.7 KiB; ~2× margin).
const SUBSCRIBE_SCRATCH: usize = 12 * 1024;

/// Max `trades` rows whose `seqId` is chain-checked per push; rows
/// beyond this are still counted as messages (OKX batches trades in
/// small clusters — 16 is generous).
pub const MAX_TRADE_ROWS: usize = 16;

// ---------------------------------------------------------------
// Subscription kinds for the core-net SubTable
// ---------------------------------------------------------------

/// Subscription-table tag (mirrors [`OkxChannel`] + free sentinel).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OkxSubKind {
    /// `bbo-tbt`.
    Bbo = 0,
    /// `trades`.
    Trades = 1,
    /// `mark-price`.
    Mark = 2,
    /// `funding-rate`.
    Funding = 3,
    /// `books`.
    Books = 4,
    /// `opt-summary` (M2.3; family-keyed).
    OptSummary = 5,
    /// Slot free.
    None = 255,
}

impl ReqKind for OkxSubKind {
    const FREE: Self = OkxSubKind::None;
}

impl OkxSubKind {
    #[inline]
    const fn from_channel(c: OkxChannel) -> Self {
        match c {
            OkxChannel::BboTbt => OkxSubKind::Bbo,
            OkxChannel::Trades => OkxSubKind::Trades,
            OkxChannel::MarkPrice => OkxSubKind::Mark,
            OkxChannel::FundingRate => OkxSubKind::Funding,
            OkxChannel::Books => OkxSubKind::Books,
            OkxChannel::OptSummary => OkxSubKind::OptSummary,
        }
    }
}

/// Subscription-table capacity (≥ [`MAX_SUB_ARGS`]).
pub const SUB_CAP: usize = MAX_SUB_ARGS;

// ---------------------------------------------------------------
// State + outer result
// ---------------------------------------------------------------

/// Run-loop state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// TLS handshake in progress.
    Connecting,
    /// TLS ready; WebSocket opening request not yet sent.
    NeedsWsWrite,
    /// Opening request sent; awaiting `101 Switching Protocols`.
    AwaitingWsUpgrade,
    /// Upgraded — subscribed, pushes flowing.
    Steady,
    /// Peer closed.
    Closed,
}

/// How a run-loop invocation terminated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// External stop flag observed.
    Stopped,
    /// Peer closed the connection.
    Disconnected,
    /// No inbound bytes within the keepalive idle budget — caller
    /// reconnects.
    IdleTimeout,
    /// WS2 (outage §5.3): no subscription was acknowledged within the
    /// establishment budget — the session never got past the
    /// handshake, or the venue refused every arg. Caller reconnects;
    /// the backoff keeps ESCALATING (no market data moved, so the
    /// T1(b) reset predicate stays false — deliberately unlike
    /// `IdleTimeout`'s venue-quiet reset).
    EstablishTimeout,
    /// Fatal transport / protocol error (includes venue `error`
    /// events at BOOT — see the module's venue-error policy).
    Error,
}

// ---------------------------------------------------------------
// Driver
// ---------------------------------------------------------------

/// Mutable per-connection state owned by the run-loop. Preallocated
/// at construction; never reallocates in steady state.
///
/// **Single-writer invariant.** One thread drives one `Driver`; the
/// `PhantomData<UnsafeCell<()>>` marker makes it `!Sync` so sharing
/// is a compile error. `Send` stays allowed — the cli moves it onto
/// its dedicated ingress thread at boot.
pub struct Driver {
    state: State,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    last_activity_ns: u64,
    mask_counter: u64,

    /// Boot-built `instId → SymbolId` map (venue-namespaced ids).
    symbols: OkxSymbolTable,
    /// Whether `books` is subscribed (`--okx-depth`).
    depth_enabled: bool,
    /// Acknowledged subscriptions.
    subs: SubTable<OkxSubKind, SUB_CAP>,
    /// Books `seqId`/`prevSeqId` chains, indexed by symbol-table row.
    book_chains: [OkxSeqChain; OKX_MAX_SYMBOLS],
    /// Trades monotonic-`seqId` monitors, same indexing.
    trade_seqs: [TradeSeqMonitor; OKX_MAX_SYMBOLS],
    /// M2.3: configured option FAMILY strings (`opt-summary`
    /// subscribe args; `(len, bytes)` rows, `n_families` valid).
    /// Boot-set, connection-independent.
    families: [(u8, [u8; 24]); OPT_FAMILIES_MAX],
    /// Valid prefix length of `families`.
    n_families: usize,
    /// Set once the post-upgrade subscribe batch has been queued.
    subscribed: bool,
    /// WS2: PROCESS-LIFETIME flag — true once ANY subscribe ack has
    /// ever been applied by this driver. While false, venue error
    /// events stay fatal (boot fail-fast: refuse venue-blind
    /// configs); once true, they become non-fatal per-arg drops.
    /// Deliberately NOT cleared by [`Self::reset_for_reconnect`].
    subs_ever_acked: bool,
    /// WS2: establishment budget (ns from session start to the first
    /// subscribe ack) enforced by [`run`]. Defaults to
    /// [`core_net::ESTABLISH_BUDGET_NS`]; overridable for tests /
    /// operator tuning via [`Self::set_establish_budget_ns`].
    establish_budget_ns: u64,
    /// WS2 drop-log rate limiter: wall ns of the last emitted
    /// sub-drop WARN line. NOT session state (operator-terminal
    /// budget) — survives reconnects, like the Deribit gap logger's.
    drop_log_last_ns: u64,
    /// Drop increments swallowed by the rate limit since the last
    /// emitted line (surfaces as `suppressed=` on the next one).
    drop_log_suppressed: u32,
    /// `!Sync` marker — see struct doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate buffers (boot-time) and seed the handshake nonce.
    /// `symbols` maps every configured instrument; `depth_enabled`
    /// adds the `books` channel per instrument; `families` (M2.3)
    /// carries the configured option underlyings for the
    /// family-keyed `opt-summary` subscription (empty slice = no
    /// options lane). Over-long/overflowing family entries are
    /// dropped with a debug assert (the config layer caps both).
    pub fn new(
        nonce_seed: u64,
        symbols: OkxSymbolTable,
        depth_enabled: bool,
        families: &[&[u8]],
    ) -> Self {
        let sec_key = sec_websocket_key_from_seed(nonce_seed);
        let accept = expected_accept(&sec_key);
        let mut fam: [(u8, [u8; 24]); OPT_FAMILIES_MAX] = [(0, [0; 24]); OPT_FAMILIES_MAX];
        let mut n_families = 0usize;
        let mut i = 0;
        while i < families.len() {
            let f = families[i];
            if f.is_empty() || f.len() > 24 || n_families >= OPT_FAMILIES_MAX {
                debug_assert!(
                    false,
                    "family entry dropped (len/cap) — config layer caps this"
                );
                i += 1;
                continue;
            }
            fam[n_families].0 = f.len() as u8;
            fam[n_families].1[..f.len()].copy_from_slice(f);
            n_families += 1;
            i += 1;
        }
        Self {
            state: State::Connecting,
            rx: IoBuf::with_capacity(RX_BUF_SIZE),
            tx: IoBuf::with_capacity(TX_BUF_SIZE),
            sec_key,
            expected_accept_val: accept,
            last_activity_ns: 0,
            mask_counter: 0,
            symbols,
            depth_enabled,
            subs: SubTable::new(),
            book_chains: [OkxSeqChain::new(); OKX_MAX_SYMBOLS],
            trade_seqs: [TradeSeqMonitor::new(); OKX_MAX_SYMBOLS],
            families: fam,
            n_families,
            subscribed: false,
            subs_ever_acked: false,
            establish_budget_ns: core_net::ESTABLISH_BUDGET_NS,
            drop_log_last_ns: 0,
            drop_log_suppressed: 0,
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// WS2: override the establishment budget (tests use millisecond
    /// budgets; production keeps the default).
    #[inline]
    pub fn set_establish_budget_ns(&mut self, ns: u64) {
        self.establish_budget_ns = ns;
    }

    /// Current state (metrics + tests).
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// Force a state in tests.
    #[cfg(test)]
    pub(crate) fn set_state(&mut self, s: State) {
        self.state = s;
    }

    /// Acknowledged-subscription count.
    #[inline]
    pub fn sub_count(&self) -> usize {
        self.subs.count()
    }

    /// Reset per-connection state for a reconnect. Subscriptions and
    /// integrity chains are connection-scoped: tables clear, chains
    /// re-arm for fresh snapshots. `subs_ever_acked` (WS2 boot/
    /// reconnect discriminator), the establishment budget and the
    /// drop-log rate limiter are process-lifetime — untouched here.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
        self.subs.clear();
        let mut i = 0;
        while i < OKX_MAX_SYMBOLS {
            self.book_chains[i].reset_await_snapshot();
            self.trade_seqs[i] = TradeSeqMonitor::new();
            i += 1;
        }
        self.subscribed = false;
    }
}

// ---------------------------------------------------------------
// drive_one — single-tick state machine advance
// ---------------------------------------------------------------

/// Pump the transport once and advance the state machine. Zero-alloc
/// once the handshake has completed.
///
/// * `producer`: this venue's Tick lane. A full ring drops the tick
///   and bumps `IngressStatus::ring_drops`.
/// * `status`: per-ingress observability slot; this thread is its
///   single writer.
pub fn drive_one<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    flush_tx(transport, drv)?;
    fill_rx(transport, drv)?;

    match drv.state {
        State::Connecting => {}
        State::NeedsWsWrite => {
            write_handshake_to_tx(drv, host, path)?;
            drv.state = State::AwaitingWsUpgrade;
        }
        State::AwaitingWsUpgrade => {
            advance_ws_upgrade(drv, status)?;
            if drv.state == State::Steady {
                queue_subscribe_all(drv)?;
            }
        }
        State::Steady => {
            drain_ws_frames(drv, producer, status, capture)?;
        }
        State::Closed => {}
    }

    flush_tx(transport, drv)?;
    Ok(())
}

/// Bump `Connecting → NeedsWsWrite` once the transport is TLS-ready.
#[inline]
pub fn note_transport_ready(drv: &mut Driver, status: Status) {
    match status {
        Status::Ready if drv.state == State::Connecting => {
            drv.state = State::NeedsWsWrite;
        }
        Status::Closed => {
            drv.state = State::Closed;
        }
        _ => {}
    }
}

// ---------------------------------------------------------------
// tx / handshake helpers (template shape — see ingress-rpc)
// ---------------------------------------------------------------

fn flush_tx<T: Transport>(transport: &mut T, drv: &mut Driver) -> io::Result<()> {
    if drv.tx.is_empty() {
        return Ok(());
    }
    let mut written = 0;
    while written < drv.tx.len() {
        match transport.write(&drv.tx.filled()[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    if written == drv.tx.len() {
        drv.tx.clear();
    } else if written > 0 {
        drv.tx.consume(written);
    }
    Ok(())
}

fn fill_rx<T: Transport>(transport: &mut T, drv: &mut Driver) -> io::Result<()> {
    loop {
        if drv.rx.free_mut().is_empty() {
            break;
        }
        match transport.read(drv.rx.free_mut()) {
            Ok(0) => {
                drv.state = State::Closed;
                break;
            }
            Ok(n) => drv.rx.advance(n),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_handshake_to_tx(drv: &mut Driver, host: &[u8], path: &[u8]) -> io::Result<()> {
    let dst = drv.tx.free_mut();
    let n = write_client_handshake(dst, host, path, &drv.sec_key)
        .map_err(|_| io::Error::other("ws handshake buffer too small"))?;
    drv.tx.advance(n);
    Ok(())
}

fn advance_ws_upgrade(drv: &mut Driver, status: &IngressStatus) -> io::Result<()> {
    match read_server_handshake(drv.rx.filled()) {
        HandshakeResult::Incomplete => Ok(()),
        HandshakeResult::Upgraded {
            accept_start,
            accept_end,
            header_end,
        } => {
            let got = &drv.rx.filled()[accept_start..accept_end];
            if got.len() != drv.expected_accept_val.len()
                || !constant_time_eq(got, &drv.expected_accept_val)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Sec-WebSocket-Accept mismatch",
                ));
            }
            drv.rx.consume(header_end);
            drv.state = State::Steady;
            // D7: publish Up exactly at the upgrade→Steady edge.
            status.set_state(IngressState::Up);
            // D5: the 101 response is inbound activity.
            let now = now_ns();
            drv.last_activity_ns = now;
            status.touch_activity(now);
            status.add_bytes(header_end as u64);
            Ok(())
        }
        HandshakeResult::Malformed => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed server handshake",
        )),
    }
}

// ---------------------------------------------------------------
// Subscribe batching
// ---------------------------------------------------------------

/// Build the full `(channel × instrument)` arg set from the symbol
/// table, gated on the discovered `instType` (8e — replaces the
/// retired `-SWAP` suffix hack): `mark-price` applies to derivatives
/// (`Swap` | `Futures`), `funding-rate` to `Swap` only. `books` rides
/// behind `depth_enabled`.
fn build_sub_args<'a>(
    symbols: &'a OkxSymbolTable,
    depth_enabled: bool,
    families: &'a [(u8, [u8; 24])],
    out: &mut [Option<SubArg<'a>>; MAX_SUB_ARGS],
) -> usize {
    let mut n = 0;
    // M2.3: one family-keyed `opt-summary` arg per configured option
    // underlying (the write_op key branch renders `instFamily`).
    let mut f = 0;
    while f < families.len() {
        let (len, ref bytes) = families[f];
        out[n] = Some(SubArg {
            channel: OkxChannel::OptSummary,
            inst_id: &bytes[..len as usize],
        });
        n += 1;
        f += 1;
    }
    let mut i = 0;
    while let Some((inst, _sym, inst_type)) = symbols.get(i) {
        out[n] = Some(SubArg {
            channel: OkxChannel::BboTbt,
            inst_id: inst,
        });
        n += 1;
        // M2.2: capped-chain OPTION rows are bbo-tbt ONLY — no
        // trades/mark/funding/books (the mark/IV `opt-summary`
        // stream arrives at M2.3).
        if inst_type == OkxInstType::Option {
            i += 1;
            continue;
        }
        out[n] = Some(SubArg {
            channel: OkxChannel::Trades,
            inst_id: inst,
        });
        n += 1;
        if matches!(inst_type, OkxInstType::Swap | OkxInstType::Futures) {
            out[n] = Some(SubArg {
                channel: OkxChannel::MarkPrice,
                inst_id: inst,
            });
            n += 1;
        }
        if inst_type == OkxInstType::Swap {
            out[n] = Some(SubArg {
                channel: OkxChannel::FundingRate,
                inst_id: inst,
            });
            n += 1;
        }
        if depth_enabled {
            out[n] = Some(SubArg {
                channel: OkxChannel::Books,
                inst_id: inst,
            });
            n += 1;
        }
        i += 1;
    }
    n
}

/// Queue the single batched subscribe op for every configured pair.
fn queue_subscribe_all(drv: &mut Driver) -> io::Result<()> {
    debug_assert!(
        !drv.subscribed,
        "subscribe batch must be queued exactly once"
    );
    let mut args_buf: [Option<SubArg<'_>>; MAX_SUB_ARGS] = [None; MAX_SUB_ARGS];
    let n_args = build_sub_args(
        &drv.symbols,
        drv.depth_enabled,
        &drv.families[..drv.n_families],
        &mut args_buf,
    );
    if n_args == 0 {
        return Err(io::Error::other("okx: no instruments configured"));
    }
    // Collapse Option wrapper into a contiguous prefix slice.
    let mut args: [SubArg<'_>; MAX_SUB_ARGS] = [SubArg {
        channel: OkxChannel::BboTbt,
        inst_id: b"",
    }; MAX_SUB_ARGS];
    let mut i = 0;
    while i < n_args {
        // Every slot below n_args was just written by build_sub_args.
        args[i] = args_buf[i].expect("contiguous prefix");
        i += 1;
    }
    let mut scratch = [0u8; SUBSCRIBE_SCRATCH];
    let len = write_subscribe_batch(&mut scratch, &args[..n_args])
        .ok_or_else(|| io::Error::other("okx: subscribe scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..len])?;
    drv.subscribed = true;
    Ok(())
}

/// Queue an unsubscribe+subscribe pair for one `(books, instId)` —
/// the §6.2 resync action after a chain break (fresh snapshot).
fn queue_books_resync(drv: &mut Driver, sym_idx: usize) -> io::Result<()> {
    let Some((inst, _sym, _inst_type)) = drv.symbols.get(sym_idx) else {
        debug_assert!(false, "resync for unknown symbol row {sym_idx}");
        return Ok(());
    };
    let args = [SubArg {
        channel: OkxChannel::Books,
        inst_id: inst,
    }];
    let mut scratch = [0u8; 256];
    let n = write_unsubscribe_batch(&mut scratch, &args)
        .ok_or_else(|| io::Error::other("okx: resync scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..n])?;
    let n = write_subscribe_batch(&mut scratch, &args)
        .ok_or_else(|| io::Error::other("okx: resync scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..n])?;
    Ok(())
}

// ---------------------------------------------------------------
// Frame drain + dispatch
// ---------------------------------------------------------------

/// Per-push `trades` scan result (phase-1 output; `Copy`).
#[derive(Copy, Clone)]
struct TradeScan {
    rows_parsed: u32,
    rows_rejected: u32,
    seq_ids: [i64; MAX_TRADE_ROWS],
    n_seq: u8,
}

/// Phase-1 dispatch outcome — everything pre-parsed while the rx
/// borrow is live, applied after it ends (template pattern).
#[derive(Copy, Clone)]
enum Dispatch {
    /// Unparseable / unclassifiable — one rejection.
    Nothing,
    /// Keepalive answer / unsubscribe ack — activity only.
    Quiet,
    /// Subscribe ack for one arg.
    SubAck { id: SubId, kind: OkxSubKind },
    /// Venue error event. Fatal at boot; a non-fatal per-arg drop on
    /// reconnect (WS2 — module doc). `sym` = the instrument the error
    /// msg names, resolved against the table (`SYMBOL_ID_NONE` when
    /// the error names none).
    VenueError { code: u32, sym: u32 },
    /// `bbo-tbt` push became a Tick.
    Bbo { tick: Tick },
    /// `trades` push scanned.
    Trades { sym_idx: u8, scan: TradeScan },
    /// M2.3: family-wide `opt-summary` push walked + captured in
    /// phase 1 (capture-only; nothing reaches the engine ring).
    OptSummaries { scan: OptScan },
    /// `mark-price` / `funding-rate` push validated.
    Slow,
    /// `books` push header.
    Book { sym_idx: u8, prev: i64, seq: i64 },
}

fn drain_ws_frames<C: Capture>(
    drv: &mut Driver,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    loop {
        let read_result = ws_read_frame(drv.rx.filled());
        match read_result {
            WsReadResult::Incomplete => return Ok(()),
            WsReadResult::Malformed => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed ws frame",
                ));
            }
            WsReadResult::Frame { header, payload } => {
                let total = header.header_len as usize + header.payload_len as usize;
                debug_assert!(total <= drv.rx.filled().len());

                if header.masked {
                    let mask = header.mask;
                    let start = payload.start;
                    let end = payload.end;
                    ws_unmask_in_place(&mut drv.rx.filled_mut()[start..end], mask);
                }

                match header.opcode {
                    WsOpcode::Text | WsOpcode::Binary => {
                        handle_data_frame(
                            drv,
                            payload.start..payload.end,
                            producer,
                            status,
                            capture,
                        )?;
                    }
                    WsOpcode::Ping => {
                        let mask = ws_mask_from_counter(drv.mask_counter);
                        drv.mask_counter = drv.mask_counter.wrapping_add(1);
                        let start = payload.start;
                        let end = payload.end;
                        let plen = end - start;
                        let mut scratch = [0u8; 125];
                        debug_assert!(plen <= scratch.len());
                        scratch[..plen].copy_from_slice(&drv.rx.filled()[start..end]);
                        let dst = drv.tx.free_mut();
                        if let Ok(n) = ws_write_pong(dst, &scratch[..plen], mask) {
                            drv.tx.advance(n);
                        }
                    }
                    WsOpcode::Pong => {}
                    WsOpcode::Close => {
                        drv.state = State::Closed;
                    }
                    WsOpcode::Continuation => {
                        // OKX does not fragment public pushes; drop
                        // rather than allocate a reassembly buffer.
                    }
                }

                // D5: every completed frame is inbound activity.
                let now = now_ns();
                drv.last_activity_ns = now;
                status.touch_activity(now);
                status.add_bytes(total as u64);
                drv.rx.consume(total);

                if drv.state == State::Closed {
                    return Ok(());
                }
            }
        }
    }
}

/// Walk the `trades` rows of one push. Rows are sliced at successive
/// `"tradeId":"` markers; each slice parses independently (`px`/`sz`/
/// `side`/`ts`/`seqId` all follow `tradeId` within a row). Each parsed
/// row is captured as a `ChannelId::Trade` event (§6.5): `venue_seq` =
/// trade `seqId`, `venue_time_ms` from the venue ts, `v0` = px ×1e6,
/// `v1` = qty ×1e6 negated for sell-side prints.
/// M2.3 `opt-summary` phase-1 scan result (`Copy`).
#[derive(Copy, Clone)]
struct OptScan {
    rows_parsed: u32,
    rows_rejected: u32,
}

/// Walk one FAMILY-wide `opt-summary` push (M2.3): one object per
/// `"instId":"` marker; rows for instruments outside the table (the
/// unsubscribed rest of the family) skip for free; rows for OUR
/// option syms parse via [`crate::parse_opt_summary_row`] and are
/// captured as [`OptSummary`] records right here in phase 1 (the
/// trades-scanner pattern — capture is a separate borrow).
fn scan_opt_summaries<C: Capture>(
    payload: &[u8],
    symbols: &OkxSymbolTable,
    capture: &mut C,
) -> OptScan {
    const MARKER: &[u8] = b"\"instId\":\"";
    let mut scan = OptScan {
        rows_parsed: 0,
        rows_rejected: 0,
    };
    let mut at = 0usize;
    while let Some(off) = memchr::memmem::find(&payload[at..], MARKER) {
        let row_start = at + off;
        let next = memchr::memmem::find(&payload[row_start + MARKER.len()..], MARKER)
            .map(|o| row_start + MARKER.len() + o);
        let row_end = next.unwrap_or(payload.len());
        let row = &payload[row_start..row_end];
        // instId value (immediately after the marker).
        let id_start = MARKER.len();
        if let Some(rel_q) = memchr::memchr(b'"', &row[id_start..]) {
            let inst = &row[id_start..id_start + rel_q];
            if let Some(sym) = symbols.lookup(inst) {
                match crate::parse_opt_summary_row(row) {
                    Some(f) => {
                        capture.opt_summary(&OptSummary::new(
                            now_ns(),
                            VenueId::Okx,
                            sym,
                            0, // no mark px / OI on this venue's channel
                            0,
                            f.mark_iv_1e9,
                            f.fwd_px_1e9,
                            0,
                            f.delta_1e9,
                            f.gamma_1e9,
                            f.vega_1e6,
                            f.theta_1e6,
                        ));
                        scan.rows_parsed += 1;
                    }
                    None => {
                        scan.rows_rejected += 1;
                        capture.parse_reject(now_ns(), row);
                    }
                }
            }
        }
        at = row_end;
    }
    scan
}

fn scan_trades<C: Capture>(payload: &[u8], sym: u32, capture: &mut C) -> TradeScan {
    const MARKER: &[u8] = b"\"tradeId\":\"";
    let mut scan = TradeScan {
        rows_parsed: 0,
        rows_rejected: 0,
        seq_ids: [0; MAX_TRADE_ROWS],
        n_seq: 0,
    };
    let mut at = 0usize;
    while let Some(off) = memchr::memmem::find(&payload[at..], MARKER) {
        let row_start = at + off;
        let next = memchr::memmem::find(&payload[row_start + MARKER.len()..], MARKER)
            .map(|o| row_start + MARKER.len() + o);
        let row_end = next.unwrap_or(payload.len());
        match parse_trade(&payload[row_start..row_end], sym) {
            Some(t) => {
                scan.rows_parsed += 1;
                if (scan.n_seq as usize) < MAX_TRADE_ROWS {
                    scan.seq_ids[scan.n_seq as usize] = t.seq_id;
                    scan.n_seq += 1;
                }
                let signed_qty = if t.side == 1 { -t.qty_1e6 } else { t.qty_1e6 };
                capture.event(&ChannelEvent::new(
                    now_ns(),
                    VenueId::Okx,
                    ChannelId::Trade,
                    sym,
                    t.seq_id as u64,
                    t.ts_ns / 1_000_000,
                    t.px_1e6,
                    signed_qty,
                ));
            }
            None => {
                scan.rows_rejected += 1;
                // Tap the exact rejected row slice — the §6.5 raw-tap
                // differential audit consumes these.
                capture.parse_reject(now_ns(), &payload[row_start..row_end]);
            }
        }
        at = row_end;
    }
    scan
}

fn handle_data_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    // Retained for the phase-2 reject re-borrow (Range is not Copy).
    let reject_range = payload_range.clone();
    // Phase 1: immutable borrow of rx (+ symbols) — classify, resolve
    // the instrument, pre-parse into a Copy dispatch value. Capture
    // hooks that need parsed values fire here (events); the raw tap
    // sees every data payload pre-classify.
    let dispatch: Dispatch = {
        let payload = &drv.rx.filled()[payload_range];
        capture.raw_frame(now_ns(), payload);
        match classify(payload) {
            OkxMsgKind::Pong => Dispatch::Quiet,
            OkxMsgKind::UnsubAck => Dispatch::Quiet,
            OkxMsgKind::Error(code) => Dispatch::VenueError {
                code,
                // WS2: best-effort — the failing instrument is named
                // inside the msg TEXT for the 60018 class.
                sym: extract_error_inst_id(payload)
                    .and_then(|inst| drv.symbols.lookup(inst))
                    .unwrap_or(SYMBOL_ID_NONE),
            },
            OkxMsgKind::SubAck => match ack_channel(payload) {
                // M2.3: the opt-summary ack arg is FAMILY-keyed.
                Some(OkxChannel::OptSummary) => match crate::extract_inst_family(payload) {
                    Some(fam) => Dispatch::SubAck {
                        id: sub_id_of(OkxChannel::OptSummary, fam),
                        kind: OkxSubKind::OptSummary,
                    },
                    None => Dispatch::Nothing,
                },
                Some(ch) => match extract_inst_id(payload) {
                    Some(inst) => Dispatch::SubAck {
                        id: sub_id_of(ch, inst),
                        kind: OkxSubKind::from_channel(ch),
                    },
                    None => Dispatch::Nothing,
                },
                None => Dispatch::Nothing,
            },
            // M2.3: opt-summary is FAMILY-keyed (no arg instId) and
            // multi-row — walked + captured in phase 1, per-row
            // instId → sym lookups against the table (rows for
            // unsubscribed family members skip for free).
            OkxMsgKind::Data(OkxChannel::OptSummary) => Dispatch::OptSummaries {
                scan: scan_opt_summaries(payload, &drv.symbols, capture),
            },
            OkxMsgKind::Data(channel) => {
                match extract_inst_id(payload).and_then(|inst| {
                    drv.symbols
                        .lookup(inst)
                        .map(|sym| (sym, drv.symbols.index_of(sym)))
                }) {
                    Some((sym, Some(sym_idx))) => match channel {
                        OkxChannel::BboTbt => match parse_bbo(payload, sym) {
                            Some(f) => Dispatch::Bbo {
                                tick: Tick::new(
                                    now_ns(),
                                    VenueId::Okx,
                                    sym,
                                    f.seq_id as u32,
                                    Price::from_raw(f.bid_px_1e6),
                                    Qty::from_raw(f.bid_qty_1e6),
                                    Price::from_raw(f.ask_px_1e6),
                                    Qty::from_raw(f.ask_qty_1e6),
                                ),
                            },
                            None => Dispatch::Nothing,
                        },
                        OkxChannel::Trades => Dispatch::Trades {
                            sym_idx: sym_idx as u8,
                            scan: scan_trades(payload, sym, capture),
                        },
                        OkxChannel::OptSummary => {
                            // Handled by the dedicated family-keyed
                            // arm above — unreachable here.
                            debug_assert!(false, "opt-summary reached the instId path");
                            Dispatch::Nothing
                        }
                        OkxChannel::MarkPrice => match crate::parse_mark_price(payload, sym) {
                            Some(m) => {
                                // §6.5 capture: v0 = mark px ×1e6.
                                capture.event(&ChannelEvent::new(
                                    now_ns(),
                                    VenueId::Okx,
                                    ChannelId::Mark,
                                    sym,
                                    0,
                                    m.ts_ns / 1_000_000,
                                    m.mark_px_1e6,
                                    0,
                                ));
                                Dispatch::Slow
                            }
                            None => Dispatch::Nothing,
                        },
                        OkxChannel::FundingRate => {
                            match crate::parse_funding_rate(payload, sym) {
                                Some(fr) => {
                                    // §6.5 capture: v0 = rate ×1e9,
                                    // v1 = next funding time (ms).
                                    capture.event(&ChannelEvent::new(
                                        now_ns(),
                                        VenueId::Okx,
                                        ChannelId::Funding,
                                        sym,
                                        0,
                                        fr.ts_ns / 1_000_000,
                                        fr.funding_rate_1e9,
                                        (fr.funding_time_ns / 1_000_000) as i64,
                                    ));
                                    Dispatch::Slow
                                }
                                None => Dispatch::Nothing,
                            }
                        }
                        OkxChannel::Books => match parse_book_header(payload, sym) {
                            Some(b) => {
                                // §6.5 capture: venue_seq = seqId,
                                // v0 = prevSeqId — the offline audit
                                // re-derives chain breaks from these.
                                capture.event(&ChannelEvent::new(
                                    now_ns(),
                                    VenueId::Okx,
                                    ChannelId::Book,
                                    sym,
                                    b.seq_id as u64,
                                    0,
                                    b.prev_seq_id,
                                    0,
                                ));
                                Dispatch::Book {
                                    sym_idx: sym_idx as u8,
                                    prev: b.prev_seq_id,
                                    seq: b.seq_id,
                                }
                            }
                            None => Dispatch::Nothing,
                        },
                    },
                    // Data for an instrument we never configured —
                    // a mapping bug or venue noise; count it.
                    _ => Dispatch::Nothing,
                }
            }
            OkxMsgKind::Unknown => Dispatch::Nothing,
        }
    };

    // Phase 2: mutable applies. The rx borrow above has ended.
    match dispatch {
        Dispatch::Nothing => {
            status.inc_parse_errors();
            // Tap the rejected payload (re-borrow is safe: dispatch is
            // Copy and the rx buffer is untouched since phase 1).
            capture.parse_reject(now_ns(), &drv.rx.filled()[reject_range]);
        }
        Dispatch::Quiet => {}
        Dispatch::SubAck { id, kind } => {
            status.add_msgs(1);
            // WS2: any applied ack proves the config is not venue-
            // blind — venue errors become non-fatal drops from here
            // on, for the rest of the PROCESS (survives reconnects).
            drv.subs_ever_acked = true;
            match drv.subs.insert(id, kind) {
                Ok(()) => {}
                Err(SubErr::ReservedId) => {}
                Err(SubErr::Full) => {
                    debug_assert!(false, "okx sub table full at SUB_CAP={SUB_CAP}");
                }
            }
        }
        Dispatch::VenueError { code, sym } => {
            // T1(a) (outage 2026-08-27 §5.2): the parsed numeric code
            // was discarded at this boundary for six days of outage —
            // record it first-wins so a later session-death log line
            // names it (post-settlement expect 60018-class instId
            // errors). Recorded on BOTH paths below: on the drop path
            // it is evidence, not an error verdict.
            status.note_venue_err_code(code);
            if !drv.subs_ever_acked {
                // BOOT fail-fast (unchanged doctrine): the first-ever
                // subscribe of a config drew an error before any ack —
                // misconfiguration; refuse to run venue-blind. Crash
                // loudly in debug, surface a session error in release
                // (reconnect w/ backoff, operator sees it).
                status.note_session_err(
                    core_metrics::ERR_SITE_VENUE_ERROR,
                    core_metrics::io_kind_code(io::ErrorKind::InvalidData),
                );
                debug_assert!(false, "okx venue error event at boot, code={code}");
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "okx venue error event",
                ));
            }
            // WS2 NON-FATAL DROP (outage §5.2): the venue processes
            // batched args individually — this error rejected ONE arg
            // (expired instrument on reconnect, or the §5.4 chronic
            // arg); everything else on the session keeps flowing. The
            // venue itself refused the subscription, so there is
            // nothing to unsubscribe: count it, pair it with a
            // capture event (§6.6), name it on stderr, carry on.
            status.inc_sub_drops();
            capture.event(&ChannelEvent::new(
                now_ns(),
                VenueId::Okx,
                ChannelId::SubDrop,
                sym,
                0,
                0,
                code as i64,
                -1, // OKX errors do not reliably name the channel
            ));
            log_sub_drop_rate_limited(drv, sym, code);
        }
        Dispatch::Bbo { tick } => {
            status.add_msgs(1);
            status.add_ticks(1);
            // §6.5 capture BEFORE the push — a ring-dropped tick must
            // still reach the replay log (the audit pairs capture
            // counts with ring_drops_total).
            capture.tick(&tick);
            // D4: a full ring is data loss — count it, never block.
            if producer.try_push(tick).is_err() {
                status.inc_ring_drops();
            }
        }
        Dispatch::Trades { sym_idx, scan } => {
            status.add_msgs(scan.rows_parsed as u64);
            status.add_ticks(scan.rows_parsed as u64);
            let mut r = 0;
            while r < scan.rows_rejected {
                status.inc_parse_errors();
                r += 1;
            }
            let mut i = 0;
            while i < scan.n_seq as usize {
                if drv.trade_seqs[sym_idx as usize].apply(scan.seq_ids[i])
                    == TradeSeqOutcome::Regression
                {
                    status.inc_gaps();
                }
                i += 1;
            }
        }
        Dispatch::OptSummaries { scan } => {
            // Rows already captured in phase 1; count messages +
            // per-row rejects (family rows outside our table were
            // free skips, not rejects).
            status.add_msgs(scan.rows_parsed as u64);
            status.add_ticks(scan.rows_parsed as u64);
            let mut r = 0;
            while r < scan.rows_rejected {
                status.inc_parse_errors();
                r += 1;
            }
        }
        Dispatch::Slow => {
            status.add_msgs(1);
            status.add_ticks(1);
        }
        Dispatch::Book { sym_idx, prev, seq } => {
            status.add_msgs(1);
            status.add_ticks(1);
            match drv.book_chains[sym_idx as usize].apply(prev, seq) {
                ChainOutcome::Init
                | ChainOutcome::Chained
                | ChainOutcome::IdleHeartbeat
                | ChainOutcome::Reset => {}
                ChainOutcome::Gap => {
                    status.inc_gaps();
                    status.inc_resubscribes();
                    queue_books_resync(drv, sym_idx as usize)?;
                }
            }
        }
    }
    Ok(())
}

/// Channel named in a subscribe/unsubscribe ack's `arg`.
#[inline]
fn ack_channel(payload: &[u8]) -> Option<OkxChannel> {
    if memchr::memmem::find(payload, b"\"channel\":\"bbo-tbt\"").is_some() {
        return Some(OkxChannel::BboTbt);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"trades\"").is_some() {
        return Some(OkxChannel::Trades);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"mark-price\"").is_some() {
        return Some(OkxChannel::MarkPrice);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"funding-rate\"").is_some() {
        return Some(OkxChannel::FundingRate);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"books\"").is_some() {
        return Some(OkxChannel::Books);
    }
    // M2.3 — the pitfall-11 catch of this slice: the first live smoke
    // tapped every opt-summary SubAck as a parse reject because this
    // arm was missing (classify knew the channel; this fn did not).
    if memchr::memmem::find(payload, b"\"channel\":\"opt-summary\"").is_some() {
        return Some(OkxChannel::OptSummary);
    }
    None
}

// ---------------------------------------------------------------
// WS2 sub-drop logging (rate-limited, zero-alloc)
// ---------------------------------------------------------------

/// Minimum interval between emitted sub-drop WARN lines (operator-
/// terminal budget; the 1:1 evidence channel is the paired
/// `ChannelId::SubDrop` capture event, not the log — same contract
/// as the Deribit gap logger).
const DROP_LOG_INTERVAL_NS: u64 = 1_000_000_000;

/// Render `v` as decimal ASCII into the tail of `scratch`.
#[inline]
fn fmt_u64(mut v: u64, scratch: &mut [u8; 20]) -> &[u8] {
    let mut i = scratch.len();
    loop {
        i -= 1;
        scratch[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &scratch[i..]
}

/// Render `v` as `0x`-prefixed lowercase hex (SymbolId display form —
/// matches the audit tool's `sym=0x...` rendering).
#[inline]
fn fmt_hex_u32(v: u32, scratch: &mut [u8; 12]) -> &[u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    scratch[0] = b'0';
    scratch[1] = b'x';
    let mut i = 2;
    let mut shift = 32;
    while shift > 0 {
        shift -= 4;
        scratch[i] = HEX[((v >> shift) & 0xF) as usize];
        i += 1;
    }
    &scratch[..10]
}

/// Rate-limited (1 line / [`DROP_LOG_INTERVAL_NS`]) WS2 sub-drop WARN
/// line. Zero-alloc: rendered into stack scratch, one `write_all` to
/// stderr (interleaves with the cli's tracing lines at line
/// granularity). Drops inside the rate-limit window surface as
/// `suppressed=` on the next emitted line. Best-effort: a failed
/// stderr write never affects the session (counter + capture event
/// already landed). Cold path — fires only on venue rejections.
fn log_sub_drop_rate_limited(drv: &mut Driver, sym: u32, code: u32) {
    let now = now_ns();
    if now.wrapping_sub(drv.drop_log_last_ns) < DROP_LOG_INTERVAL_NS {
        drv.drop_log_suppressed = drv.drop_log_suppressed.saturating_add(1);
        return;
    }
    let suppressed = drv.drop_log_suppressed;
    drv.drop_log_last_ns = now;
    drv.drop_log_suppressed = 0;

    // `WARN ingress-okx: sub-drop sym=<hex> code=<u32>
    //  suppressed=<n> ts_ns=<n>\n`
    let mut buf = [0u8; 128];
    let mut n = 0usize;
    let mut ok = true;
    let put = |buf: &mut [u8; 128], n: &mut usize, ok: &mut bool, src: &[u8]| {
        if *n + src.len() <= buf.len() {
            buf[*n..*n + src.len()].copy_from_slice(src);
            *n += src.len();
        } else {
            *ok = false;
        }
    };
    put(
        &mut buf,
        &mut n,
        &mut ok,
        b"WARN ingress-okx: sub-drop sym=",
    );
    let mut h = [0u8; 12];
    put(&mut buf, &mut n, &mut ok, fmt_hex_u32(sym, &mut h));
    put(&mut buf, &mut n, &mut ok, b" code=");
    let mut d1 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, fmt_u64(code as u64, &mut d1));
    put(&mut buf, &mut n, &mut ok, b" suppressed=");
    let mut d2 = [0u8; 20];
    put(
        &mut buf,
        &mut n,
        &mut ok,
        fmt_u64(suppressed as u64, &mut d2),
    );
    put(&mut buf, &mut n, &mut ok, b" ts_ns=");
    let mut d3 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, fmt_u64(now, &mut d3));
    put(&mut buf, &mut n, &mut ok, b"\n");
    debug_assert!(ok, "drop log scratch sized for the worst case");
    if ok {
        let mut err = std::io::stderr().lock();
        let _ = std::io::Write::write_all(&mut err, &buf[..n]);
    }
}

// ---------------------------------------------------------------
// Top-level driver — mio-driven loop
// ---------------------------------------------------------------

/// Stop flag raised by external threads for graceful shutdown.
pub type StopFlag = AtomicBool;

/// Run the OKX ingress loop until `stop` is set or the session ends.
/// Reconnect is the caller's responsibility.
///
/// `keepalive` drives the venue-specific probe: on `SendPing` the
/// literal `ping` text frame is queued and flushed (OKX cuts idle
/// connections at 30 s; cli configures a 25 s interval); on
/// `Reconnect` the session is dead by policy → [`RunResult::IdleTimeout`].
#[allow(clippy::too_many_arguments)]
pub fn run<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    token: mio::Token,
    stop: &StopFlag,
    status: &IngressStatus,
    keepalive: &mut Keepalive,
    capture: &mut C,
) -> RunResult {
    let session_start_ns = now_ns();
    keepalive.reset();

    // T1(a) (outage 2026-08-27 §5.5): every fatal site below records
    // its site + io-kind into the status slot before returning
    // `RunResult::Error`; the cli names the triple on its
    // `run-loop returned` line. First-error-wins, cleared by the
    // caller — see `IngressStatus::take_last_err`.
    if let Err(e) = transport.register(poll.registry(), token) {
        status.note_session_err(
            core_metrics::ERR_SITE_REGISTER,
            core_metrics::io_kind_code(e.kind()),
        );
        return RunResult::Error;
    }
    let mut last_interest = transport.interest();

    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = poll.poll(events, Some(std::time::Duration::from_millis(50))) {
            status.note_session_err(
                core_metrics::ERR_SITE_POLL,
                core_metrics::io_kind_code(e.kind()),
            );
            return RunResult::Error;
        }

        for ev in events.iter() {
            if ev.token() != token {
                continue;
            }
            let transport_status = match transport.pump(ev) {
                Ok(s) => s,
                Err(e) => {
                    status.note_session_err(
                        core_metrics::ERR_SITE_PUMP,
                        core_metrics::io_kind_code(e.kind()),
                    );
                    return RunResult::Error;
                }
            };
            note_transport_ready(drv, transport_status);
        }

        // Tight inner drain (see ingress-polymarket for rationale).
        loop {
            let n_before = producer.len();
            let state_before = drv.state();
            if let Err(e) = drive_one(transport, drv, host, path, producer, status, capture) {
                status.note_session_err(
                    core_metrics::ERR_SITE_DRIVE,
                    core_metrics::io_kind_code(e.kind()),
                );
                return RunResult::Error;
            }
            if drv.state() == State::Closed {
                return RunResult::Disconnected;
            }
            if producer.len() == n_before && drv.state() == state_before {
                break;
            }
        }

        // §6.5: staged capture reaches disk within the flush interval
        // even on quiet feeds (one clock read + one branch per poll
        // iteration; ~50 ms cadence).
        let flush_now = now_ns();
        capture.maybe_flush(flush_now);

        // WS2 (outage §5.3): establishment budget — deliberately NOT
        // gated on `Steady`. A session wedged pre-upgrade (blackholed
        // SYN under the non-blocking connect) or sitting in `Steady`
        // with zero acknowledged subscriptions (kept "active" only by
        // pong replies) is dead by policy once the budget expires.
        // One compare per poll iteration, clock read shared with the
        // flush above.
        if core_net::establishment_expired(
            flush_now,
            session_start_ns,
            drv.sub_count(),
            drv.establish_budget_ns,
        ) {
            status.note_session_err(
                core_metrics::ERR_SITE_ESTABLISH,
                core_metrics::io_kind_code(io::ErrorKind::TimedOut),
            );
            return RunResult::EstablishTimeout;
        }

        // Keepalive: OKX wants a literal `ping` text frame; the
        // venue's `pong` (or any push) refreshes the activity clock.
        if drv.state() == State::Steady {
            let now = now_ns();
            let last = if drv.last_activity_ns == 0 {
                session_start_ns
            } else {
                drv.last_activity_ns
            };
            match keepalive.poll(now, last) {
                KeepaliveAction::None => {}
                KeepaliveAction::SendPing => {
                    if queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, PING_PAYLOAD)
                        .is_err()
                        || flush_tx(transport, drv).is_err()
                    {
                        // T1(a): site only — the two sources have
                        // distinct error types; the site is the
                        // diagnostic payload here.
                        status.note_session_err(core_metrics::ERR_SITE_KEEPALIVE, 0);
                        return RunResult::Error;
                    }
                    keepalive.mark_ping_sent(now);
                }
                KeepaliveAction::Reconnect => return RunResult::IdleTimeout,
            }
        }

        let cur = transport.interest();
        if cur != last_interest {
            if let Err(e) = transport.reregister(poll.registry(), token) {
                status.note_session_err(
                    core_metrics::ERR_SITE_REREGISTER,
                    core_metrics::io_kind_code(e.kind()),
                );
                return RunResult::Error;
            }
            last_interest = cur;
        }
    }

    RunResult::Stopped
}

// ---------------------------------------------------------------
// Tests — TestTransport-driven
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_net::{
        expected_accept as expected_accept_pub, sec_websocket_key_from_seed as sec_key_pub,
        KeepaliveCfg, TestTransport,
    };
    use core_ring::Ring;
    use core_types::NullCapture;

    /// Venue-namespaced test symbol (venue byte 2 = Okx, ordinal 1).
    const SYM_BTC: u32 = (2 << 24) | 1;

    fn test_symbols() -> OkxSymbolTable {
        let mut t = OkxSymbolTable::new();
        t.insert(b"BTC-USDT", SYM_BTC, OkxInstType::Spot).unwrap();
        t.insert(b"ETH-USD-SWAP", (2 << 24) | 2, OkxInstType::Swap)
            .unwrap();
        t
    }

    /// Test capture recording OptSummary records (M2.3).
    struct RecCap {
        records: Vec<OptSummary>,
        rejects: u32,
    }
    impl Capture for RecCap {
        fn opt_summary(&mut self, o: &OptSummary) {
            self.records.push(*o);
        }
        fn parse_reject(&mut self, _ts: u64, _p: &[u8]) {
            self.rejects += 1;
        }
    }

    #[test]
    fn opt_summary_ack_is_recognized_and_family_keyed() {
        // The M2.3 live-smoke catch, pinned: an opt-summary SubAck
        // must resolve a family-keyed SubId — never fall to Nothing
        // (which taps it as a parse reject).
        let ack = br#"{"event":"subscribe","arg":{"channel":"opt-summary","instFamily":"BTC-USD"},"connId":"x"}"#;
        assert_eq!(ack_channel(ack), Some(OkxChannel::OptSummary));
        assert_eq!(crate::extract_inst_family(ack), Some(&b"BTC-USD"[..]));
    }

    #[test]
    fn opt_summary_scan_captures_ours_skips_foreign_rejects_bad() {
        let mut t = test_symbols();
        t.insert(
            b"BTC-USD-260327-100000-C",
            (2 << 24) | 513,
            OkxInstType::Option,
        )
        .unwrap();
        // Family push: one OUR row, one foreign-family row (skip), one
        // OUR row malformed (reject). Foreign malformed rows are also
        // free skips — only OUR rows can reject.
        let payload = br#"{"arg":{"channel":"opt-summary","instFamily":"BTC-USD"},"data":[
          {"instId":"BTC-USD-260327-100000-C","markVol":"0.6543","fwdPx":"77300.12","deltaBS":"0.512","gammaBS":"1.234e-5","thetaBS":"-85.3","vegaBS":"152.3","ts":"1774598400123"},
          {"instId":"BTC-USD-260327-90000-P","markVol":"0.7","fwdPx":"77300.12","deltaBS":"-0.2","gammaBS":"0.00001","thetaBS":"-40.0","vegaBS":"100.0"},
          {"instId":"BTC-USD-260327-100000-C","markVol":"","fwdPx":"77300.12","deltaBS":"0.5","gammaBS":"0","thetaBS":"0","vegaBS":"0"}
        ]}"#;
        let mut cap = RecCap {
            records: Vec::new(),
            rejects: 0,
        };
        let scan = scan_opt_summaries(payload, &t, &mut cap);
        assert_eq!(scan.rows_parsed, 1);
        assert_eq!(scan.rows_rejected, 1); // the malformed OUR row
        assert_eq!(cap.records.len(), 1);
        assert_eq!(cap.rejects, 1);
        let r = &cap.records[0];
        assert_eq!(r.sym, (2 << 24) | 513);
        assert_eq!(r.venue, VenueId::Okx as u8);
        assert_eq!(r.flags, 0); // no mark px / OI on this venue
        assert_eq!(r.mark_px_1e9, 0);
        assert_eq!(r.open_interest_1e6, 0);
        assert_eq!(r.mark_iv_1e9, 654_300_000);
        assert_eq!(r.underlying_px_1e9, 77_300_120_000_000);
        assert_eq!(r.delta_1e9, 512_000_000);
        assert_eq!(r.theta_1e6, -85_300_000);
    }

    #[test]
    fn sub_args_prepend_family_opt_summary_args() {
        let t = test_symbols();
        let fams: [(u8, [u8; 24]); 1] = {
            let mut f = [(0u8, [0u8; 24]); 1];
            f[0].0 = 7;
            f[0].1[..7].copy_from_slice(b"BTC-USD");
            f
        };
        let mut buf: [Option<SubArg<'_>>; MAX_SUB_ARGS] = [None; MAX_SUB_ARGS];
        let n = build_sub_args(&t, false, &fams, &mut buf);
        // 1 family arg + spot 2 + swap 4.
        assert_eq!(n, 7);
        let first = buf[0].unwrap();
        assert_eq!(first.channel, OkxChannel::OptSummary);
        assert_eq!(first.inst_id, b"BTC-USD");
    }

    #[test]
    fn sub_args_option_rows_are_bbo_tbt_only() {
        // M2.2: an OPTION row contributes exactly one bbo-tbt arg —
        // no trades/mark/funding, and depth never touches it.
        let mut t = test_symbols(); // spot: 2 args; swap: 4 args (no depth)
        t.insert(
            b"BTC-USD-260327-100000-C",
            (2 << 24) | 513,
            OkxInstType::Option,
        )
        .unwrap();
        let mut buf: [Option<SubArg<'_>>; MAX_SUB_ARGS] = [None; MAX_SUB_ARGS];
        let n = build_sub_args(&t, false, &[], &mut buf);
        // spot(bbo,trades)=2 + swap(bbo,trades,mark,funding)=4 + option(bbo)=1.
        assert_eq!(n, 7);
        let opt_args: Vec<_> = buf[..n]
            .iter()
            .map(|a| a.unwrap())
            .filter(|a| a.inst_id == b"BTC-USD-260327-100000-C")
            .collect();
        assert_eq!(opt_args.len(), 1);
        assert_eq!(opt_args[0].channel, OkxChannel::BboTbt);
        // Depth adds books for non-option rows only.
        let n_depth = build_sub_args(&t, true, &[], &mut buf);
        assert_eq!(n_depth, 9); // +1 books each for spot + swap, none for the option
        let opt_args_depth = buf[..n_depth]
            .iter()
            .map(|a| a.unwrap())
            .filter(|a| a.inst_id == b"BTC-USD-260327-100000-C")
            .count();
        assert_eq!(opt_args_depth, 1);
    }

    fn steady_driver(depth: bool) -> Driver {
        let mut d = Driver::new(7, test_symbols(), depth, &[]);
        d.set_state(State::Steady);
        d.subscribed = true;
        d
    }

    fn generous_keepalive() -> Keepalive {
        Keepalive::new(KeepaliveCfg {
            ping_interval_ns: u64::MAX / 4,
            idle_timeout_ns: u64::MAX / 2,
        })
    }

    fn build_server_response(accept: &[u8; 28]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(256);
        out.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
        out.extend_from_slice(b"Upgrade: websocket\r\n");
        out.extend_from_slice(b"Connection: Upgrade\r\n");
        out.extend_from_slice(b"Sec-WebSocket-Accept: ");
        out.extend_from_slice(accept);
        out.extend_from_slice(b"\r\n\r\n");
        out
    }

    /// Unmasked (server→client) WS text frame around `body`.
    fn wrap_text_frame(body: &[u8], out: &mut [u8]) -> usize {
        out[0] = 0x81;
        if body.len() <= 125 {
            out[1] = body.len() as u8;
            out[2..2 + body.len()].copy_from_slice(body);
            2 + body.len()
        } else {
            assert!(body.len() <= u16::MAX as usize);
            out[1] = 126;
            let len_be = (body.len() as u16).to_be_bytes();
            out[2] = len_be[0];
            out[3] = len_be[1];
            out[4..4 + body.len()].copy_from_slice(body);
            4 + body.len()
        }
    }

    /// Unmask every masked client frame in `buf`, concatenating the
    /// payloads (test-side inspection of our own tx stream).
    fn unmask_client_frames(mut buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while buf.len() >= 2 {
            let masked = buf[1] & 0x80 != 0;
            let mut len = (buf[1] & 0x7F) as usize;
            let mut at = 2;
            if len == 126 {
                len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                at = 4;
            }
            assert!(masked, "client frames must be masked");
            let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
            at += 4;
            for i in 0..len {
                out.push(buf[at + i] ^ mask[i & 3]);
            }
            buf = &buf[at + len..];
        }
        out
    }

    fn ring_pair() -> (
        Producer<Tick, TICK_RING_CAP>,
        core_ring::Consumer<Tick, TICK_RING_CAP>,
    ) {
        let ring = Ring::<Tick, TICK_RING_CAP>::new();
        ring.split()
    }

    #[test]
    fn driver_starts_in_connecting() {
        let d = Driver::new(1, test_symbols(), false, &[]);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert!(!d.subscribed);
    }

    #[test]
    fn note_transport_ready_advances_and_closes() {
        let mut d = Driver::new(1, test_symbols(), false, &[]);
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite);
        note_transport_ready(&mut d, Status::Closed);
        assert_eq!(d.state(), State::Closed);
    }

    #[test]
    fn handshake_completes_and_batched_subscribe_is_emitted() {
        let mut t = TestTransport::with_capacity(65536);
        let mut d = Driver::new(42, test_symbols(), true, &[]);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(
            &mut t,
            &mut d,
            b"ws.okx.com",
            b"/ws/v5/public",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        let mut scratch = vec![0u8; 65536];
        let _ = t.drain_outgoing(&mut scratch);

        let key = sec_key_pub(42);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(
            &mut t,
            &mut d,
            b"ws.okx.com",
            b"/ws/v5/public",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(d.state(), State::Steady);
        assert!(d.subscribed);
        assert_eq!(status.state(), IngressState::Up);

        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        // One batched op containing every configured pair.
        assert!(memchr::memmem::find(&body, b"\"op\":\"subscribe\"").is_some());
        assert!(
            memchr::memmem::find(&body, b"{\"channel\":\"bbo-tbt\",\"instId\":\"BTC-USDT\"}")
                .is_some()
        );
        assert!(memchr::memmem::find(
            &body,
            b"{\"channel\":\"trades\",\"instId\":\"ETH-USD-SWAP\"}"
        )
        .is_some());
        // -SWAP gating: mark/funding only on the swap instrument.
        assert!(memchr::memmem::find(
            &body,
            b"{\"channel\":\"funding-rate\",\"instId\":\"ETH-USD-SWAP\"}"
        )
        .is_some());
        assert!(memchr::memmem::find(
            &body,
            b"{\"channel\":\"funding-rate\",\"instId\":\"BTC-USDT\"}"
        )
        .is_none());
        // Depth enabled: books present.
        assert!(
            memchr::memmem::find(&body, b"{\"channel\":\"books\",\"instId\":\"BTC-USDT\"}")
                .is_some()
        );
        // Exactly one op frame (batching, not per-arg ops).
        assert_eq!(
            memchr::memmem::find_iter(&body, b"\"op\":\"subscribe\"").count(),
            1
        );
    }

    #[test]
    fn data_advances_ticks_total_but_control_does_not() {
        // T1(b) (outage 2026-08-27 §5.3): the backoff predicate reads
        // `ticks_total` — a subscribe ack must NOT advance it, a bbo
        // push must. Failure mode covered: a session that only ever
        // receives control frames keeps ticks_total at 0.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let ack = br#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"connId":"x"}"#;
        let mut frame = [0u8; 256];
        let n = wrap_text_frame(ack, &mut frame);
        t.inject_incoming(&frame[..n]);
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.msgs_total(), 1, "ack counts as a message");
        assert_eq!(status.ticks_total(), 0, "ack must not count as a tick");

        let bbo = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["111.06","5","0","2"]],"bids":[["111.05","7","0","2"]],"ts":"1670324386802","seqId":363996337}]}"#;
        let mut frame2 = [0u8; 512];
        let n2 = wrap_text_frame(bbo, &mut frame2);
        t.inject_incoming(&frame2[..n2]);
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.ticks_total(), 1, "bbo push is a tick");
        let _ = cons.try_pop().expect("tick must be pushed");
    }

    #[test]
    fn sub_ack_registers_in_table() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let ack = br#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"connId":"x"}"#;
        let mut frame = [0u8; 256];
        let n = wrap_text_frame(ack, &mut frame);
        t.inject_incoming(&frame[..n]);

        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(d.sub_count(), 1);
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.parse_errors_total(), 0);
        assert_eq!(
            d.subs.kind_of(sub_id_of(OkxChannel::BboTbt, b"BTC-USDT")),
            Some(OkxSubKind::Bbo)
        );
    }

    #[test]
    fn bbo_push_emits_tick_with_venue_byte() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let bbo = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["111.06","5","0","2"]],"bids":[["111.05","7","0","2"]],"ts":"1670324386802","seqId":363996337}]}"#;
        let mut frame = [0u8; 512];
        let n = wrap_text_frame(bbo, &mut frame);
        t.inject_incoming(&frame[..n]);

        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.ring_drops_total(), 0);

        let tick = cons.try_pop().expect("tick must be pushed");
        assert_eq!(tick.sym, SYM_BTC);
        assert_eq!(tick.venue, VenueId::Okx as u8);
        assert_eq!(tick.venue_seq, 363_996_337u32);
        assert_eq!(tick.bid_px.raw(), 111_050_000);
        assert_eq!(tick.ask_px.raw(), 111_060_000);
        assert_eq!(tick.bid_qty.raw(), 7_000_000);
        assert_eq!(tick.ask_qty.raw(), 5_000_000);
        assert!(tick.ts_ns > 0);
    }

    #[test]
    fn unknown_instrument_counts_parse_error() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let bbo = br#"{"arg":{"channel":"bbo-tbt","instId":"DOGE-USDT"},"data":[{"asks":[["1","1","0","1"]],"bids":[["1","1","0","1"]],"ts":"1000","seqId":1}]}"#;
        let mut frame = [0u8; 512];
        let n = wrap_text_frame(bbo, &mut frame);
        t.inject_incoming(&frame[..n]);

        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.parse_errors_total(), 1);
        assert_eq!(status.msgs_total(), 0);
        assert!(cons.try_pop().is_none());
    }

    #[test]
    fn venue_error_event_fails_the_session_at_boot() {
        // WS2: BOOT fail-fast preserved — before any ack has ever
        // been applied, a venue error is fatal (refuse venue-blind).
        // debug builds crash on the debug_assert (fail-fast); the
        // release-path behaviour is a session error → reconnect.
        if cfg!(debug_assertions) {
            return;
        }
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let err = br#"{"event":"error","code":"60012","msg":"Invalid request"}"#;
        let mut frame = [0u8; 256];
        let n = wrap_text_frame(err, &mut frame);
        t.inject_incoming(&frame[..n]);

        let e = drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(status.sub_drops_total(), 0, "boot errors are not drops");
    }

    /// Records every ChannelEvent — WS2 drop-event pinning.
    #[derive(Default)]
    struct EventRecCap {
        events: Vec<ChannelEvent>,
    }
    impl Capture for EventRecCap {
        fn event(&mut self, e: &ChannelEvent) {
            self.events.push(*e);
        }
    }

    #[test]
    fn venue_error_after_ack_is_nonfatal_per_arg_drop() {
        // WS2 (outage §5.2): once any subscribe ack has been applied,
        // a venue error drops ONE arg — counter + paired SubDrop
        // capture event — and the session keeps running.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let mut cap = EventRecCap::default();

        let ack = br#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"connId":"x"}"#;
        let err = br#"{"event":"error","code":"60018","msg":"Wrong URL or channel:bbo-tbt,instId:ETH-USD-SWAP doesn't exist. Please use the correct URL, channel and parameters referring to API document.","connId":"x"}"#;
        let bbo = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["111.06","5","0","2"]],"bids":[["111.05","7","0","2"]],"ts":"1670324386802","seqId":363996337}]}"#;
        let mut frame = [0u8; 512];
        for payload in [&ack[..], &err[..], &bbo[..]] {
            let n = wrap_text_frame(payload, &mut frame);
            t.inject_incoming(&frame[..n]);
        }

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap)
            .expect("post-ack venue error must be non-fatal");
        assert!(d.subs_ever_acked);
        assert_eq!(status.sub_drops_total(), 1);
        assert_eq!(
            status.parse_errors_total(),
            0,
            "a drop is not a parse error"
        );
        // The paired SubDrop event names the instrument + code.
        let drop = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::SubDrop as u8)
            .expect("SubDrop event captured");
        assert_eq!(
            drop.sym,
            (2 << 24) | 2,
            "error msg instId resolved to the swap row"
        );
        assert_eq!(drop.v0, 60018);
        assert_eq!(drop.v1, -1);
        // T1 evidence recorded without a fatal verdict.
        let snap = status.take_last_err();
        assert_eq!(snap.venue_code, 60018);
        assert_eq!(snap.site, 0, "no session-error site on the drop path");
        // The session still moves data.
        let tick = cons.try_pop().expect("bbo after the drop still flows");
        assert_eq!(tick.sym, SYM_BTC);
    }

    #[test]
    fn expired_instrument_reconnect_keeps_statics_flowing() {
        // The WS2 exit test as named by the plan: an option row that
        // expired mid-run draws errors on the RECONNECT session; the
        // spot/swap args keep flowing on the same connection.
        let mut opt_symbols = test_symbols();
        opt_symbols
            .insert(
                b"BTC-USD-260828-45000-C",
                (2 << 24) | 513,
                OkxInstType::Option,
            )
            .unwrap();
        let mut t = TestTransport::with_capacity(16384);
        let mut d = Driver::new(7, opt_symbols, false, &[]);
        d.set_state(State::Steady);
        d.subscribed = true;
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        // "Boot" session: one applied ack arms the process-lifetime
        // discriminator.
        let ack = br#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"connId":"x"}"#;
        let mut frame = [0u8; 512];
        let n = wrap_text_frame(ack, &mut frame);
        t.inject_incoming(&frame[..n]);
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert!(d.subs_ever_acked);

        // Reconnect: connection-scoped state clears, the WS2 flag
        // survives.
        d.reset_for_reconnect(9);
        assert!(
            d.subs_ever_acked,
            "boot/reconnect discriminator survives reset"
        );
        assert_eq!(d.sub_count(), 0);
        d.set_state(State::Steady);
        d.subscribed = true;

        // The reconnect session: statics ack, the expired option
        // errors (60018), a spot bbo pushes.
        let err = br#"{"event":"error","code":"60018","msg":"Wrong URL or channel:bbo-tbt,instId:BTC-USD-260828-45000-C doesn't exist.","connId":"x"}"#;
        let bbo = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["111.06","5","0","2"]],"bids":[["111.05","7","0","2"]],"ts":"1670324386802","seqId":363996337}]}"#;
        for payload in [&ack[..], &err[..], &bbo[..]] {
            let n = wrap_text_frame(payload, &mut frame);
            t.inject_incoming(&frame[..n]);
        }
        let mut cap = EventRecCap::default();
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap)
            .expect("expired-instrument error on reconnect must not kill the session");
        assert_eq!(status.sub_drops_total(), 1);
        let drop = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::SubDrop as u8)
            .expect("SubDrop event captured");
        assert_eq!(
            drop.sym,
            (2 << 24) | 513,
            "drop names the expired option row"
        );
        let tick = cons
            .try_pop()
            .expect("spot keeps flowing beside the dead option");
        assert_eq!(tick.sym, SYM_BTC);
        assert_eq!(
            d.sub_count(),
            1,
            "the static ack registered on the reconnect session"
        );
    }

    #[test]
    fn venue_error_naming_unknown_instrument_drops_with_sym_none() {
        // A post-ack error whose msg names no (or a foreign)
        // instrument still drops non-fatally, attributed to
        // SYMBOL_ID_NONE.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        d.subs_ever_acked = true;
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = EventRecCap::default();

        let err = br#"{"event":"error","code":"60012","msg":"Invalid request"}"#;
        let mut frame = [0u8; 256];
        let n = wrap_text_frame(err, &mut frame);
        t.inject_incoming(&frame[..n]);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.sub_drops_total(), 1);
        let drop = &cap.events[0];
        assert_eq!(drop.sym, SYMBOL_ID_NONE);
        assert_eq!(drop.v0, 60012);
    }

    #[test]
    fn establish_timeout_fires_when_no_sub_is_ever_acked() {
        // WS2 (outage §5.3): a session that never produces a
        // confirmed subscription dies at the establishment budget —
        // NOT gated on Steady (here the driver never even upgrades).
        let mut t = TestTransport::with_capacity(4096);
        let mut d = Driver::new(1, test_symbols(), false, &[]);
        d.set_establish_budget_ns(50_000_000); // 50 ms test budget
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = generous_keepalive();

        let res = run(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        assert_eq!(res, RunResult::EstablishTimeout);
        let snap = status.take_last_err();
        assert_eq!(snap.site, core_metrics::ERR_SITE_ESTABLISH);
    }

    #[test]
    fn establish_timeout_disarmed_by_first_ack() {
        // Once one subscription is acknowledged the establishment
        // budget never fires — the tiny 1 ns budget here would trip
        // on the first iteration otherwise; instead the session lives
        // on to the ordinary idle timeout.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        d.set_establish_budget_ns(1);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: 150_000_000,
        });

        let ack = br#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"connId":"x"}"#;
        let mut frame = [0u8; 256];
        let n = wrap_text_frame(ack, &mut frame);
        t.inject_incoming(&frame[..n]);

        let res = run(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        assert_eq!(
            res,
            RunResult::IdleTimeout,
            "ack disarms the establishment budget"
        );
        assert_eq!(d.sub_count(), 1);
    }

    #[test]
    fn books_gap_queues_unsubscribe_subscribe_resync() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(true);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        // Snapshot inits the chain…
        let snap = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"asks":[["1","1","0","1"]],"bids":[["1","1","0","1"]],"ts":"1000","checksum":0,"prevSeqId":-1,"seqId":10}]}"#;
        let mut frame = [0u8; 512];
        let n = wrap_text_frame(snap, &mut frame);
        t.inject_incoming(&frame[..n]);
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.gaps_total(), 0);
        let mut scratch = vec![0u8; 16384];
        let _ = t.drain_outgoing(&mut scratch);

        // …then a chain break (prev 99 ≠ last 10).
        let broken = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update","data":[{"asks":[["1","1","0","1"]],"bids":[],"ts":"2000","checksum":0,"prevSeqId":99,"seqId":100}]}"#;
        let n = wrap_text_frame(broken, &mut frame);
        t.inject_incoming(&frame[..n]);
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();

        assert_eq!(status.gaps_total(), 1);
        assert_eq!(status.resubscribes_total(), 1);
        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        let unsub_at =
            memchr::memmem::find(&body, b"\"op\":\"unsubscribe\"").expect("unsubscribe queued");
        let sub_at =
            memchr::memmem::find(&body, b"\"op\":\"subscribe\"").expect("subscribe queued");
        assert!(unsub_at < sub_at, "unsubscribe must precede subscribe");
        assert!(
            memchr::memmem::find(&body, b"{\"channel\":\"books\",\"instId\":\"BTC-USDT\"}")
                .is_some()
        );
    }

    #[test]
    fn trade_seq_regression_counts_gap() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let t1 = br#"{"arg":{"channel":"trades","instId":"BTC-USDT"},"data":[{"instId":"BTC-USDT","tradeId":"1","px":"1.0","sz":"1.0","side":"buy","ts":"1000","seqId":50}]}"#;
        let t2 = br#"{"arg":{"channel":"trades","instId":"BTC-USDT"},"data":[{"instId":"BTC-USDT","tradeId":"2","px":"1.0","sz":"1.0","side":"sell","ts":"1001","seqId":40}]}"#;
        let mut frame = [0u8; 512];
        let n = wrap_text_frame(t1, &mut frame);
        t.inject_incoming(&frame[..n]);
        let n = wrap_text_frame(t2, &mut frame);
        t.inject_incoming(&frame[..n]);

        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.msgs_total(), 2);
        assert_eq!(status.gaps_total(), 1, "seq 50 → 40 is a regression");
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn trades_multi_row_push_counts_each_row() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let multi = br#"{"arg":{"channel":"trades","instId":"BTC-USDT"},"data":[{"instId":"BTC-USDT","tradeId":"1","px":"1.0","sz":"1.0","side":"buy","ts":"1000","seqId":50},{"instId":"BTC-USDT","tradeId":"2","px":"1.1","sz":"2.0","side":"sell","ts":"1001","seqId":51}]}"#;
        let mut frame = [0u8; 1024];
        let n = wrap_text_frame(multi, &mut frame);
        t.inject_incoming(&frame[..n]);

        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.msgs_total(), 2, "both rows counted");
        assert_eq!(status.gaps_total(), 0);
    }

    /// Records every hook invocation — pins the §6.5 capture-site
    /// semantics without touching the filesystem.
    #[derive(Default)]
    struct CountingCapture {
        ticks: u32,
        events: u32,
        raw_frames: u32,
        rejects: u32,
        flushes: u32,
        last_event_channel: u8,
    }

    impl core_types::Capture for CountingCapture {
        fn tick(&mut self, _t: &Tick) {
            self.ticks += 1;
        }
        fn event(&mut self, e: &ChannelEvent) {
            self.events += 1;
            self.last_event_channel = e.channel;
        }
        fn raw_frame(&mut self, _ts_ns: u64, _payload: &[u8]) {
            self.raw_frames += 1;
        }
        fn parse_reject(&mut self, _ts_ns: u64, _payload: &[u8]) {
            self.rejects += 1;
        }
        fn maybe_flush(&mut self, _now_ns: u64) {
            self.flushes += 1;
        }
    }

    #[test]
    fn capture_hooks_fire_at_documented_sites() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = CountingCapture::default();

        // One bbo (tick + raw), one trades push with one good + one
        // broken row (event + reject + raw), one garbage frame
        // (reject + raw).
        let bbo = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["1.1","5","0","2"]],"bids":[["1.0","7","0","2"]],"ts":"1670324386802","seqId":1}]}"#;
        let trades = br#"{"arg":{"channel":"trades","instId":"BTC-USDT"},"data":[{"instId":"BTC-USDT","tradeId":"1","px":"1.0","sz":"2.0","side":"sell","ts":"1000","seqId":50},{"instId":"BTC-USDT","tradeId":"2","broken":true}]}"#;
        let garbage = br#"{"what":"ever"}"#;
        let mut frame = [0u8; 1024];
        for payload in [&bbo[..], &trades[..], &garbage[..]] {
            let n = wrap_text_frame(payload, &mut frame);
            t.inject_incoming(&frame[..n]);
        }

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(cap.raw_frames, 3, "every data payload is tapped");
        assert_eq!(cap.ticks, 1, "bbo captured as tick");
        assert_eq!(cap.events, 1, "one good trade row captured");
        assert_eq!(cap.last_event_channel, core_types::ChannelId::Trade as u8);
        assert_eq!(
            cap.rejects, 2,
            "broken trade row + garbage frame both tapped as rejects"
        );
        assert_eq!(status.parse_errors_total(), 2);
        // Tick still captured when the ring is full: fill it, resend.
        while prod
            .try_push(Tick::new(
                1,
                VenueId::Okx,
                SYM_BTC,
                1,
                Price::from_raw(1),
                Qty::from_raw(1),
                Price::from_raw(2),
                Qty::from_raw(1),
            ))
            .is_ok()
        {}
        let n = wrap_text_frame(bbo, &mut frame);
        t.inject_incoming(&frame[..n]);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.ticks, 2, "ring-dropped tick still captured");
        assert_eq!(status.ring_drops_total(), 1);
    }

    #[test]
    fn pong_is_quiet_activity() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let mut frame = [0u8; 64];
        let n = wrap_text_frame(b"pong", &mut frame);
        t.inject_incoming(&frame[..n]);

        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.msgs_total(), 0);
        assert_eq!(status.parse_errors_total(), 0);
        assert!(
            status.last_activity_ns() > 0,
            "pong refreshes the idle clock"
        );
    }

    #[test]
    fn reset_for_reconnect_clears_connection_state() {
        let mut d = steady_driver(true);
        d.subs
            .insert(sub_id_of(OkxChannel::BboTbt, b"BTC-USDT"), OkxSubKind::Bbo)
            .unwrap();
        assert_eq!(d.book_chains[0].apply(-1, 5), ChainOutcome::Init);
        d.reset_for_reconnect(9);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert!(!d.subscribed);
        // Chains re-armed: an update without snapshot is a gap again.
        assert_eq!(d.book_chains[0].apply(5, 6), ChainOutcome::Gap);
    }

    #[test]
    fn run_returns_idle_timeout_on_dead_session() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: 2,
        });

        let res = run(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        assert_eq!(res, RunResult::IdleTimeout);
    }

    #[test]
    fn run_emits_literal_ping_before_idle_timeout() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        // Ping due immediately; death after ~150 ms of silence.
        let mut ka = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: 150_000_000,
        });

        let res = run(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        assert_eq!(res, RunResult::IdleTimeout);
        let mut scratch = [0u8; 4096];
        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        assert!(
            memchr::memmem::find(&body, b"ping").is_some(),
            "literal ping text frame must have been sent"
        );
    }

    #[test]
    fn run_disconnects_on_server_close() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        // Server-side Close frame (unmasked, empty payload).
        t.inject_incoming(&[0x88, 0x00]);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = generous_keepalive();

        let res = run(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        assert_eq!(res, RunResult::Disconnected);
        assert_eq!(status.bytes_total(), 2);
    }
}
