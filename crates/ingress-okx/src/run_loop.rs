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
//! `core_net::SubTable`; venue `error` events are **fatal** for the
//! session (fail-fast: a rejected subscribe means misconfiguration —
//! surface it, reconnect with backoff, let the operator see it).
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
    ws_unmask_in_place, ws_write_pong, HandshakeResult, IoBuf, Keepalive, KeepaliveAction,
    ReqKind, Status, SubErr, SubId, SubTable, Transport, WsOpcode, WsReadResult,
};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{Price, Qty, Tick, VenueId};

use crate::{
    classify, extract_inst_id, parse_bbo, parse_book_header, parse_trade, sub_id_of,
    write_subscribe_batch, write_unsubscribe_batch, ChainOutcome, OkxChannel, OkxMsgKind,
    OkxSeqChain, OkxSymbolTable, SubArg, TradeSeqMonitor, TradeSeqOutcome, OKX_MAX_SYMBOLS,
    PING_PAYLOAD,
};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Rx buffer: a `books` 400-level snapshot is ~25 KiB; 256 KiB
/// absorbs bursts of full snapshots across instruments.
pub const RX_BUF_SIZE: usize = 256 * 1024;

/// Tx buffer: handshake + one batched subscribe op (~46 B per arg ×
/// [`MAX_SUB_ARGS`]) + resync pairs + pings.
pub const TX_BUF_SIZE: usize = 8 * 1024;

/// Tick-ring capacity. Must equal `engine::TICK_RING_SIZE` — the cli
/// const-asserts the equality when wiring lanes (8a §3.3 pattern).
pub const TICK_RING_CAP: usize = 16_384;

/// Upper bound on `(channel × instrument)` subscribe args:
/// 5 channels × [`OKX_MAX_SYMBOLS`].
pub const MAX_SUB_ARGS: usize = 5 * OKX_MAX_SYMBOLS;

/// Stack scratch for one rendered subscribe batch (~46 B/arg × 80
/// args ≈ 3.7 KiB; doubled for margin).
const SUBSCRIBE_SCRATCH: usize = 8 * 1024;

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
    /// Fatal transport / protocol error (includes venue `error`
    /// events — fail-fast).
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
    /// Set once the post-upgrade subscribe batch has been queued.
    subscribed: bool,
    /// `!Sync` marker — see struct doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate buffers (boot-time) and seed the handshake nonce.
    /// `symbols` maps every configured instrument; `depth_enabled`
    /// adds the `books` channel per instrument.
    pub fn new(nonce_seed: u64, symbols: OkxSymbolTable, depth_enabled: bool) -> Self {
        let sec_key = sec_websocket_key_from_seed(nonce_seed);
        let accept = expected_accept(&sec_key);
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
            subscribed: false,
            _not_sync: ::core::marker::PhantomData,
        }
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
    /// re-arm for fresh snapshots.
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
pub fn drive_one<T: Transport>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
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
            drain_ws_frames(drv, producer, status)?;
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
    if drv.tx.len() == 0 {
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
/// table. `mark-price`/`funding-rate` only apply to perp swaps —
/// gated on the `-SWAP` instId suffix until REST discovery carries
/// `instType` (8e). `books` rides behind `depth_enabled`.
fn build_sub_args<'a>(
    symbols: &'a OkxSymbolTable,
    depth_enabled: bool,
    out: &mut [Option<SubArg<'a>>; MAX_SUB_ARGS],
) -> usize {
    let mut n = 0;
    let mut i = 0;
    while let Some((inst, _sym)) = symbols.get(i) {
        out[n] = Some(SubArg { channel: OkxChannel::BboTbt, inst_id: inst });
        n += 1;
        out[n] = Some(SubArg { channel: OkxChannel::Trades, inst_id: inst });
        n += 1;
        if inst.ends_with(b"-SWAP") {
            out[n] = Some(SubArg { channel: OkxChannel::MarkPrice, inst_id: inst });
            n += 1;
            out[n] = Some(SubArg { channel: OkxChannel::FundingRate, inst_id: inst });
            n += 1;
        }
        if depth_enabled {
            out[n] = Some(SubArg { channel: OkxChannel::Books, inst_id: inst });
            n += 1;
        }
        i += 1;
    }
    n
}

/// Queue the single batched subscribe op for every configured pair.
fn queue_subscribe_all(drv: &mut Driver) -> io::Result<()> {
    debug_assert!(!drv.subscribed, "subscribe batch must be queued exactly once");
    let mut args_buf: [Option<SubArg<'_>>; MAX_SUB_ARGS] = [None; MAX_SUB_ARGS];
    let n_args = build_sub_args(&drv.symbols, drv.depth_enabled, &mut args_buf);
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
    let Some((inst, _sym)) = drv.symbols.get(sym_idx) else {
        debug_assert!(false, "resync for unknown symbol row {sym_idx}");
        return Ok(());
    };
    let args = [SubArg { channel: OkxChannel::Books, inst_id: inst }];
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
    /// Venue error event — fatal (fail-fast).
    VenueError { code: u32 },
    /// `bbo-tbt` push became a Tick.
    Bbo { tick: Tick },
    /// `trades` push scanned.
    Trades { sym_idx: u8, scan: TradeScan },
    /// `mark-price` / `funding-rate` push validated.
    Slow,
    /// `books` push header.
    Book { sym_idx: u8, prev: i64, seq: i64 },
}

fn drain_ws_frames(
    drv: &mut Driver,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
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
                        handle_data_frame(drv, payload.start..payload.end, producer, status)?;
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
/// `side`/`ts`/`seqId` all follow `tradeId` within a row).
fn scan_trades(payload: &[u8], sym: u32) -> TradeScan {
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
            }
            None => scan.rows_rejected += 1,
        }
        at = row_end;
    }
    scan
}

fn handle_data_frame(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
) -> io::Result<()> {
    // Phase 1: immutable borrow of rx (+ symbols) — classify, resolve
    // the instrument, pre-parse into a Copy dispatch value.
    let dispatch: Dispatch = {
        let payload = &drv.rx.filled()[payload_range];
        match classify(payload) {
            OkxMsgKind::Pong => Dispatch::Quiet,
            OkxMsgKind::UnsubAck => Dispatch::Quiet,
            OkxMsgKind::Error(code) => Dispatch::VenueError { code },
            OkxMsgKind::SubAck => match ack_channel(payload) {
                Some(ch) => match extract_inst_id(payload) {
                    Some(inst) => Dispatch::SubAck {
                        id: sub_id_of(ch, inst),
                        kind: OkxSubKind::from_channel(ch),
                    },
                    None => Dispatch::Nothing,
                },
                None => Dispatch::Nothing,
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
                            scan: scan_trades(payload, sym),
                        },
                        OkxChannel::MarkPrice => match crate::parse_mark_price(payload, sym) {
                            Some(_) => Dispatch::Slow,
                            None => Dispatch::Nothing,
                        },
                        OkxChannel::FundingRate => {
                            match crate::parse_funding_rate(payload, sym) {
                                Some(_) => Dispatch::Slow,
                                None => Dispatch::Nothing,
                            }
                        }
                        OkxChannel::Books => match parse_book_header(payload, sym) {
                            Some(b) => Dispatch::Book {
                                sym_idx: sym_idx as u8,
                                prev: b.prev_seq_id,
                                seq: b.seq_id,
                            },
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
        Dispatch::Nothing => status.inc_parse_errors(),
        Dispatch::Quiet => {}
        Dispatch::SubAck { id, kind } => {
            status.add_msgs(1);
            match drv.subs.insert(id, kind) {
                Ok(()) => {}
                Err(SubErr::ReservedId) => {}
                Err(SubErr::Full) => {
                    debug_assert!(false, "okx sub table full at SUB_CAP={SUB_CAP}");
                }
            }
        }
        Dispatch::VenueError { code } => {
            // Fail-fast doctrine: a venue error event means our
            // subscribe (or framing) is wrong. Crash loudly in debug,
            // surface a session error in release — the reconnect
            // path applies backoff and the operator sees it.
            debug_assert!(false, "okx venue error event, code={code}");
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "okx venue error event",
            ));
        }
        Dispatch::Bbo { tick } => {
            status.add_msgs(1);
            // D4: a full ring is data loss — count it, never block.
            if producer.try_push(tick).is_err() {
                status.inc_ring_drops();
            }
        }
        Dispatch::Trades { sym_idx, scan } => {
            status.add_msgs(scan.rows_parsed as u64);
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
        Dispatch::Slow => status.add_msgs(1),
        Dispatch::Book { sym_idx, prev, seq } => {
            status.add_msgs(1);
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
    None
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
pub fn run<T: Transport>(
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
) -> RunResult {
    let session_start_ns = now_ns();
    keepalive.reset();

    if transport.register(poll.registry(), token).is_err() {
        return RunResult::Error;
    }
    let mut last_interest = transport.interest();

    while !stop.load(Ordering::Relaxed) {
        if poll
            .poll(events, Some(std::time::Duration::from_millis(50)))
            .is_err()
        {
            return RunResult::Error;
        }

        for ev in events.iter() {
            if ev.token() != token {
                continue;
            }
            let transport_status = match transport.pump(ev) {
                Ok(s) => s,
                Err(_e) => return RunResult::Error,
            };
            note_transport_ready(drv, transport_status);
        }

        // Tight inner drain (see ingress-polymarket for rationale).
        loop {
            let n_before = producer.len();
            let state_before = drv.state();
            if drive_one(transport, drv, host, path, producer, status).is_err() {
                return RunResult::Error;
            }
            if drv.state() == State::Closed {
                return RunResult::Disconnected;
            }
            if producer.len() == n_before && drv.state() == state_before {
                break;
            }
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
                        return RunResult::Error;
                    }
                    keepalive.mark_ping_sent(now);
                }
                KeepaliveAction::Reconnect => return RunResult::IdleTimeout,
            }
        }

        let cur = transport.interest();
        if cur != last_interest {
            if transport.reregister(poll.registry(), token).is_err() {
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

    /// Venue-namespaced test symbol (venue byte 2 = Okx, ordinal 1).
    const SYM_BTC: u32 = (2 << 24) | 1;

    fn test_symbols() -> OkxSymbolTable {
        let mut t = OkxSymbolTable::new();
        t.insert(b"BTC-USDT", SYM_BTC).unwrap();
        t.insert(b"ETH-USD-SWAP", (2 << 24) | 2).unwrap();
        t
    }

    fn steady_driver(depth: bool) -> Driver {
        let mut d = Driver::new(7, test_symbols(), depth);
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
        let d = Driver::new(1, test_symbols(), false);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert!(!d.subscribed);
    }

    #[test]
    fn note_transport_ready_advances_and_closes() {
        let mut d = Driver::new(1, test_symbols(), false);
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite);
        note_transport_ready(&mut d, Status::Closed);
        assert_eq!(d.state(), State::Closed);
    }

    #[test]
    fn handshake_completes_and_batched_subscribe_is_emitted() {
        let mut t = TestTransport::with_capacity(65536);
        let mut d = Driver::new(42, test_symbols(), true);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"ws.okx.com", b"/ws/v5/public", &mut prod, &status).unwrap();
        let mut scratch = vec![0u8; 65536];
        let _ = t.drain_outgoing(&mut scratch);

        let key = sec_key_pub(42);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(&mut t, &mut d, b"ws.okx.com", b"/ws/v5/public", &mut prod, &status).unwrap();
        assert_eq!(d.state(), State::Steady);
        assert!(d.subscribed);
        assert_eq!(status.state(), IngressState::Up);

        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        // One batched op containing every configured pair.
        assert!(memchr::memmem::find(&body, b"\"op\":\"subscribe\"").is_some());
        assert!(memchr::memmem::find(&body, b"{\"channel\":\"bbo-tbt\",\"instId\":\"BTC-USDT\"}").is_some());
        assert!(memchr::memmem::find(&body, b"{\"channel\":\"trades\",\"instId\":\"ETH-USD-SWAP\"}").is_some());
        // -SWAP gating: mark/funding only on the swap instrument.
        assert!(memchr::memmem::find(&body, b"{\"channel\":\"funding-rate\",\"instId\":\"ETH-USD-SWAP\"}").is_some());
        assert!(memchr::memmem::find(&body, b"{\"channel\":\"funding-rate\",\"instId\":\"BTC-USDT\"}").is_none());
        // Depth enabled: books present.
        assert!(memchr::memmem::find(&body, b"{\"channel\":\"books\",\"instId\":\"BTC-USDT\"}").is_some());
        // Exactly one op frame (batching, not per-arg ops).
        assert_eq!(
            memchr::memmem::find_iter(&body, b"\"op\":\"subscribe\"").count(),
            1
        );
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

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
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

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
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

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.parse_errors_total(), 1);
        assert_eq!(status.msgs_total(), 0);
        assert!(cons.try_pop().is_none());
    }

    #[test]
    fn venue_error_event_fails_the_session() {
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

        let e = drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
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
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.gaps_total(), 0);
        let mut scratch = vec![0u8; 16384];
        let _ = t.drain_outgoing(&mut scratch);

        // …then a chain break (prev 99 ≠ last 10).
        let broken = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update","data":[{"asks":[["1","1","0","1"]],"bids":[],"ts":"2000","checksum":0,"prevSeqId":99,"seqId":100}]}"#;
        let n = wrap_text_frame(broken, &mut frame);
        t.inject_incoming(&frame[..n]);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();

        assert_eq!(status.gaps_total(), 1);
        assert_eq!(status.resubscribes_total(), 1);
        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        let unsub_at = memchr::memmem::find(&body, b"\"op\":\"unsubscribe\"").expect("unsubscribe queued");
        let sub_at = memchr::memmem::find(&body, b"\"op\":\"subscribe\"").expect("subscribe queued");
        assert!(unsub_at < sub_at, "unsubscribe must precede subscribe");
        assert!(memchr::memmem::find(&body, b"{\"channel\":\"books\",\"instId\":\"BTC-USDT\"}").is_some());
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

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
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

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.msgs_total(), 2, "both rows counted");
        assert_eq!(status.gaps_total(), 0);
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

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.msgs_total(), 0);
        assert_eq!(status.parse_errors_total(), 0);
        assert!(status.last_activity_ns() > 0, "pong refreshes the idle clock");
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
            &mut t, &mut d, b"h", b"/", &mut prod, &mut poll, &mut events,
            mio::Token(1), &stop, &status, &mut ka,
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
            &mut t, &mut d, b"h", b"/", &mut prod, &mut poll, &mut events,
            mio::Token(1), &stop, &status, &mut ka,
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
            &mut t, &mut d, b"h", b"/", &mut prod, &mut poll, &mut events,
            mio::Token(1), &stop, &status, &mut ka,
        );
        assert_eq!(res, RunResult::Disconnected);
        assert_eq!(status.bytes_total(), 2);
    }
}
