// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # pm15_loop — the rolling 15-minute ingress thread (PM15-2 / PM15-3)
//!
//! One thread, one `mio` poll, two sockets ([`core_net::WsConn`]):
//!
//! - **market** — `wss://ws-subscriptions-clob.polymarket.com/ws/market`
//!   with the custom features on: the bound windows' tokens, subscribed and
//!   unsubscribed as they roll; `book`, `price_change`, `tick_size_change`,
//!   `last_trade_price` and `market_resolved`. Text `PING` every 5 s.
//! - **rtds** — the Chainlink reports ([`crate::rtds`]). Text `PING` every 5 s.
//!
//! The windows come from the discovery thread ([`crate::pm15_discovery`])
//! over an SPSC ring; [`crate::rolling::PmFamilies`] decides what each one
//! means; this module turns that and the frames into the engine's
//! records, all on the PM15 lane (tick lane and event lane 8):
//!
//! | record | when |
//! |---|---|
//! | `Tick` on a leg slot | a ticking slot's touch changed (prices from the venue, sizes from its ladder, [`crate::book15`]); re-asserted unchanged at each `PONG` of the market socket; withdrawn (stale, zero) when the market session drops |
//! | `InstrumentRoll` created / settled | a window opened / resolved or was retired (`core_types::pm_roll`) |
//! | `OutcomeMeta` per leg | at creation and at every `tick_size_change` — the order law |
//! | `Mark` on the `[mark]` slot | each Chainlink report of a family's asset, once per second |
//! | `Trade` per leg | each `last_trade_price` of a ticking slot — **capture only** unless the lane mask asks for it: `v0` price ×1e6, `v1` size ×1e6, `venue_seq` 1 = BUY / 2 = SELL |
//!
//! Every record reaches the capture before its ring push (§6.5). The
//! driver ([`Pm15Driver`]) is socket-free and testable on its own; the
//! loop ([`run_pm15`]) only moves bytes and time into it.
//!
//! ## Doctrine
//!
//! Zero allocation after boot: the ladders and the tables are allocated
//! by [`Pm15Driver::new`]; frames are parsed in place in the sockets'
//! receive windows; subscribe frames are written as parts straight into
//! the send window. The only copies are the documented cold ones (a
//! window's spec into its slot, token ids into an unsubscribe). No
//! iterator on a frame path; fail-fast on a broken invariant.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::io;

use core_metrics::{IngressState, IngressStatus};
use core_net::{Backoff, WsConn, WsErr, WsNext, WsProgress};
use core_ring::{Consumer, Producer};
use core_time::{now_ns, FeedClock, WallAnchor};
use core_types::pm_roll::{
    pm_mark_sym, pm_rolling_sym, pm_window_end_ns, PM_LEG_UP, PM_PAIRS, PM_TWAP_S,
};
use core_types::{
    event_lane_bit, pack_roll_seq, ArrivalSeq, Capture, ChannelEvent, ChannelId, NsTs, Price, Qty,
    Tick, VenueId, EVENT_RING_SIZE, TICK_FLAG_STALE,
};

use crate::book15::{parse_change_row, quoted_price, Ladder15, Touch15};
use crate::gamma15::PmWindowSpec;
use crate::rolling::{PmAction, PmActions, PmFamilies, PmOffer, PmResolved, PM_SLOTS};
use crate::rtds::{parse_rtds, ChainlinkPx, RtdsFrame, RTDS_PING, RTDS_PING_NS, RTDS_SUBSCRIBE};
use crate::{scan_venue_time_ms, DEFAULT_TICK_RING_CAP};

/// The market socket's host.
pub const PM15_MARKET_HOST: &str = "ws-subscriptions-clob.polymarket.com";
/// The market socket's path.
pub const PM15_MARKET_PATH: &str = "/ws/market";
/// The market socket's keepalive cadence, ns. 5 s, as RTDS's: at 10 s the
/// venue closed the socket 4 times in a 900 s probe (`last_market_err` 8,
/// Closed) while RTDS at 5 s never dropped.
pub const PM15_MARKET_PING_NS: u64 = 5_000_000_000;
/// Capacity of the discovery → ingress spec ring.
pub const PM15_SPEC_RING: usize = 64;
/// Legs: two per slot.
pub const PM15_LEGS: usize = PM_SLOTS * 2;
/// The `mio` token of the market socket.
pub const PM15_MARKET_TOKEN: mio::Token = mio::Token(0);
/// The `mio` token of the RTDS socket.
pub const PM15_RTDS_TOKEN: mio::Token = mio::Token(1);
/// The poll timeout: the roll clock's resolution, ns-free (50 ms).
const POLL_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(50);

/// A touch no ladder can produce: the next touch is always "changed".
const TOUCH_UNSEEN: Touch15 = Touch15 {
    bid_px: -1,
    bid_sz: -1,
    ask_px: -1,
    ask_sz: -1,
};

// ---------------------------------------------------------------
// Counters
// ---------------------------------------------------------------

/// The PM15 ingress counters (names: [`PM15_COUNTER_NAMES`]).
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Pm15Counter {
    /// A window bound to its slot.
    SpecsBound = 0,
    /// A window waiting for its slot.
    SpecsPending,
    /// A window offered again.
    SpecsDuplicate,
    /// A window offered too late (over, or older than its slot's).
    SpecsStale,
    /// A window for a family that does not exist.
    SpecsRefused,
    /// CREATED rolls emitted.
    RollsCreated,
    /// SETTLED rolls emitted with a winner.
    RollsSettled,
    /// SETTLED rolls emitted UNKNOWN (retired unresolved, or contradicting).
    RollsUnknown,
    /// A `market_resolved` for no token of ours.
    ResolvedForeign,
    /// A `book` event the ladder refused.
    BookRejects,
    /// A `price_change` row that did not parse or was off the grid.
    RowRejects,
    /// A touch price off the ladder's grid.
    TouchOffGrid,
    /// `tick_size_change` events applied.
    TickChanges,
    /// Slots stopped by a tick the ladder cannot hold.
    SlotsBroken,
    /// `last_trade_price` events captured.
    Trades,
    /// Chainlink marks emitted.
    Marks,
    /// Chainlink reports repeating a second already emitted.
    MarksDuplicate,
    /// Chainlink marks judged stale on arrival.
    MarksStale,
    /// RTDS reports that did not parse.
    RtdsRejects,
    /// RTDS sessions lost.
    RtdsReconnects,
    /// Market sessions lost.
    MarketReconnects,
    /// PM15-5c: touches re-asserted unchanged at a market `PONG`.
    TouchesReasserted,
    /// PM15-5c: touches withdrawn (a stale tick) when the market session
    /// dropped.
    TouchesWithdrawn,
    /// Discovery: a window's market fetched and venue-law clean.
    DiscoveryOk,
    /// Discovery: a window's market refused by the venue law.
    DiscoveryLawRefused,
    /// Discovery: a fetch that failed (transport, status, or no market).
    DiscoveryFetchErrors,
    /// Discovery: a spec the ring could not take.
    DiscoveryRingFull,
}

/// How many [`Pm15Counter`]s there are.
pub const PM15_COUNTERS: usize = Pm15Counter::DiscoveryRingFull as usize + 1;

/// The metric suffix of each counter, in discriminant order.
pub const PM15_COUNTER_NAMES: [&str; PM15_COUNTERS] = [
    "specs_bound",
    "specs_pending",
    "specs_duplicate",
    "specs_stale",
    "specs_refused",
    "rolls_created",
    "rolls_settled",
    "rolls_unknown",
    "resolved_foreign",
    "book_rejects",
    "row_rejects",
    "touch_off_grid",
    "tick_changes",
    "slots_broken",
    "trades",
    "marks",
    "marks_duplicate",
    "marks_stale",
    "rtds_rejects",
    "rtds_reconnects",
    "market_reconnects",
    "touches_reasserted",
    "touches_withdrawn",
    "discovery_ok",
    "discovery_law_refused",
    "discovery_fetch_errors",
    "discovery_ring_full",
];

/// The counters: the ingress and the discovery threads write, the
/// metrics publisher reads (`Relaxed` — monotone tallies).
pub struct Pm15Counters {
    c: [AtomicU64; PM15_COUNTERS],
    /// Slots bound right now (a gauge).
    slots_bound: AtomicU64,
    /// The last venue-law refusal: `family << 8 | cause` (`PmLawErr` as
    /// u8), `u64::MAX` = none yet.
    last_law_refusal: AtomicU64,
    /// Why each socket last failed: `core_net::WsErr` as u8 + 1 (0 = never),
    /// `[market, rtds]`.
    last_ws_err: [AtomicU64; 2],
}

impl Pm15Counters {
    /// All zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            c: [const { AtomicU64::new(0) }; PM15_COUNTERS],
            slots_bound: AtomicU64::new(0),
            last_law_refusal: AtomicU64::new(u64::MAX),
            last_ws_err: [const { AtomicU64::new(0) }; 2],
        }
    }

    /// Record why socket `which` (0 market, 1 rtds) failed.
    #[inline]
    pub fn note_ws_err(&self, which: usize, e: core_net::WsErr) {
        self.last_ws_err[which & 1].store(e as u64 + 1, Ordering::Relaxed);
    }

    /// Why socket `which` (0 market, 1 rtds) last failed (`WsErr` as u8).
    #[must_use]
    pub fn last_ws_err(&self, which: usize) -> Option<u8> {
        match self.last_ws_err[which & 1].load(Ordering::Relaxed) {
            0 => None,
            v => Some((v - 1) as u8),
        }
    }

    /// Record a venue-law refusal of family `f`'s window.
    #[inline]
    pub fn note_law_refusal(&self, f: usize, cause: crate::gamma15::PmLawErr) {
        self.last_law_refusal
            .store(((f as u64) << 8) | cause as u64, Ordering::Relaxed);
    }

    /// The last venue-law refusal, `(family, cause)`.
    #[must_use]
    pub fn last_law_refusal(&self) -> Option<(usize, u8)> {
        let v = self.last_law_refusal.load(Ordering::Relaxed);
        if v == u64::MAX {
            return None;
        }
        Some(((v >> 8) as usize, (v & 0xFF) as u8))
    }

    /// One more of `c`.
    #[inline(always)]
    pub fn inc(&self, c: Pm15Counter) {
        self.c[c as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// The tally of counter index `i` (`0..PM15_COUNTERS`).
    #[inline]
    #[must_use]
    pub fn get(&self, i: usize) -> u64 {
        self.c[i % PM15_COUNTERS].load(Ordering::Relaxed)
    }

    /// The tally of `c`.
    #[inline]
    #[must_use]
    pub fn of(&self, c: Pm15Counter) -> u64 {
        self.get(c as usize)
    }

    /// Slots bound right now.
    #[inline]
    #[must_use]
    pub fn slots_bound(&self) -> u64 {
        self.slots_bound.load(Ordering::Relaxed)
    }
}

impl Default for Pm15Counters {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// The outputs and the wire
// ---------------------------------------------------------------

/// Where the driver's records go.
pub struct Pm15Out<'a, C: Capture> {
    /// The PM15 tick lane.
    pub tick: &'a mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    /// The PM15 event lane.
    pub event: &'a mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    /// Which channels reach the event lane (`core_types::event_lane_bit`);
    /// every event reaches the capture.
    pub event_mask: u16,
    /// The market socket's observability slot.
    pub status: &'a IngressStatus,
    /// The PM15 counters.
    pub counters: &'a Pm15Counters,
    /// The capture.
    pub capture: &'a mut C,
}

/// The lane mask the engine runs PM15 with: the roll, the order law and
/// the mark (trades stay capture-only).
pub const PM15_EVENT_MASK: u16 = event_lane_bit(ChannelId::InstrumentRoll)
    | event_lane_bit(ChannelId::OutcomeMeta)
    | event_lane_bit(ChannelId::Mark);

/// The market socket as the driver sees it.
pub trait Pm15Wire {
    /// The session is open (frames may be queued).
    fn is_open(&self) -> bool;
    /// Queue one text frame laid out as `parts`; `false` when it could not
    /// be queued (the session is then failed by its owner).
    fn queue(&mut self, parts: &[&[u8]]) -> bool;
}

impl Pm15Wire for WsConn {
    #[inline]
    fn is_open(&self) -> bool {
        WsConn::is_open(self)
    }

    #[inline]
    fn queue(&mut self, parts: &[&[u8]]) -> bool {
        self.queue_text(parts).is_ok()
    }
}

/// The market subscribe for every bound token: `{"assets_ids":[…],
/// "type":"market","custom_feature_enabled":true}`.
const SUB_HEAD: &[u8] = b"{\"assets_ids\":[";
const SUB_TAIL: &[u8] = b"],\"type\":\"market\",\"custom_feature_enabled\":true}";
const DYN_HEAD: &[u8] = b"{\"assets_ids\":[\"";
const DYN_MID: &[u8] = b"\",\"";
const DYN_SUB: &[u8] = b"\"],\"operation\":\"subscribe\",\"custom_feature_enabled\":true}";
const DYN_UNSUB: &[u8] = b"\"],\"operation\":\"unsubscribe\"}";
/// Parts of the initial subscribe: head, tail, and `"`, id, `"`, `,` per token.
const SUB_PARTS_MAX: usize = 2 + 4 * PM15_LEGS;

// ---------------------------------------------------------------
// The driver
// ---------------------------------------------------------------

/// What a market frame was (its first `event_type`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    Book,
    PriceChange,
    TickSize,
    LastTrade,
    Resolved,
    Other,
}

/// The socket-free half of the PM15 ingress (module docs).
pub struct Pm15Driver {
    fams: PmFamilies,
    acts: PmActions,
    /// One ladder per leg: index `2 · slot + leg`.
    ladders: Box<[Ladder15]>,
    /// The touch last emitted per leg.
    last: [Touch15; PM15_LEGS],
    /// The venue stamp of the touch last emitted per leg, ms: the
    /// sequencer's key for its re-assertion and its withdrawal.
    last_vt: [u64; PM15_LEGS],
    /// The arrival sequencer per leg (`Tick.venue_seq`).
    seq: [ArrivalSeq; PM15_LEGS],
    /// The newest Chainlink second emitted per family.
    last_mark_s: [u64; core_types::pm_roll::PM_ROLLING_MAX],
    market_clock: FeedClock,
    rtds_clock: FeedClock,
}

impl Pm15Driver {
    /// **Boot-only** (the ladders are allocated here). `assets[f]` is
    /// family `f`'s asset (the slug's spelling, `[polymarket] rolling`
    /// order); `stale_after_ms` the VT2 staleness of both sockets.
    #[must_use]
    pub fn new(assets: &[&[u8]], stale_after_ms: u32) -> Self {
        let mut ladders = Vec::with_capacity(PM15_LEGS);
        let mut i = 0usize;
        while i < PM15_LEGS {
            ladders.push(Ladder15::EMPTY);
            i += 1;
        }
        Self {
            fams: PmFamilies::new(assets),
            acts: PmActions::new(),
            ladders: ladders.into_boxed_slice(),
            last: [TOUCH_UNSEEN; PM15_LEGS],
            last_vt: [0; PM15_LEGS],
            seq: [ArrivalSeq::ZERO; PM15_LEGS],
            last_mark_s: [0; core_types::pm_roll::PM_ROLLING_MAX],
            market_clock: FeedClock::new(stale_after_ms),
            rtds_clock: FeedClock::new(stale_after_ms),
        }
    }

    /// The families.
    #[inline]
    #[must_use]
    pub fn families(&self) -> &PmFamilies {
        &self.fams
    }

    /// Slots bound right now (the market socket is dialled only when > 0).
    #[must_use]
    pub fn bound(&self) -> usize {
        let mut n = 0usize;
        let mut s = 0usize;
        while s < self.fams.n_slots() {
            n += usize::from(self.fams.slot(s).state != crate::rolling::PmSlotState::Empty);
            s += 1;
        }
        n
    }

    /// One window from discovery, at wall time `wall_ns`.
    pub fn offer<C: Capture>(
        &mut self,
        spec: &PmWindowSpec,
        wall_ns: u64,
        ts_ns: NsTs,
        out: &mut Pm15Out<'_, C>,
        wire: &mut impl Pm15Wire,
    ) {
        let c = match self.fams.offer(spec, wall_ns, &mut self.acts) {
            PmOffer::Bound => Pm15Counter::SpecsBound,
            PmOffer::Pending => Pm15Counter::SpecsPending,
            PmOffer::Duplicate => Pm15Counter::SpecsDuplicate,
            PmOffer::Stale => Pm15Counter::SpecsStale,
            PmOffer::Refused => Pm15Counter::SpecsRefused,
        };
        out.counters.inc(c);
        self.perform(ts_ns, out, wire);
    }

    /// The roll clock, once per loop pass.
    pub fn on_clock<C: Capture>(
        &mut self,
        wall_ns: u64,
        ts_ns: NsTs,
        out: &mut Pm15Out<'_, C>,
        wire: &mut impl Pm15Wire,
    ) {
        self.fams.on_clock(wall_ns, &mut self.acts);
        self.perform(ts_ns, out, wire);
    }

    /// The market session opened: subscribe every bound token (the
    /// venue answers each with a `book` snapshot, so every ladder starts
    /// over).
    pub fn on_market_open(&mut self, wire: &mut impl Pm15Wire) -> bool {
        self.reset_ladders();
        let mut parts: [&[u8]; SUB_PARTS_MAX] = [&[]; SUB_PARTS_MAX];
        parts[0] = SUB_HEAD;
        let mut n = 1usize;
        let mut s = 0usize;
        while s < self.fams.n_slots() {
            let b = self.fams.slot(s);
            s += 1;
            if b.state == crate::rolling::PmSlotState::Empty {
                continue;
            }
            let mut leg = 0u32;
            while leg < 2 {
                if n > 1 {
                    parts[n] = b",";
                    n += 1;
                }
                parts[n] = b"\"";
                parts[n + 1] = b.token(leg).as_bytes();
                parts[n + 2] = b"\"";
                n += 3;
                leg += 1;
            }
        }
        parts[n] = SUB_TAIL;
        n += 1;
        wire.queue(&parts[..n])
    }

    /// The market session is gone: every touch emitted is withdrawn (a
    /// stale tick, both sides zero — the member and the paper book stop
    /// trusting it now, not at the idle law's next verdict); every ladder
    /// starts over at the next open; the sequencers' and the estimator's
    /// clocks reset.
    pub fn on_market_down<C: Capture>(&mut self, ts_ns: NsTs, out: &mut Pm15Out<'_, C>) {
        let mut i = 0usize;
        while i < PM15_LEGS {
            if self.last[i] != TOUCH_UNSEEN && self.fams.slot(i / 2).ticks() {
                let none = Touch15 {
                    bid_px: 0,
                    bid_sz: 0,
                    ask_px: 0,
                    ask_sz: 0,
                };
                self.push_touch(i, none, self.last_vt[i], TICK_FLAG_STALE, ts_ns, out);
                out.counters.inc(Pm15Counter::TouchesWithdrawn);
            }
            i += 1;
        }
        self.reset_ladders();
        ArrivalSeq::reset_clocks(&mut self.seq);
        self.market_clock.reset();
    }

    /// The RTDS session is gone.
    pub fn on_rtds_down(&mut self) {
        self.rtds_clock.reset();
    }

    fn reset_ladders(&mut self) {
        let mut i = 0usize;
        while i < PM15_LEGS {
            self.ladders[i].clear();
            self.last[i] = TOUCH_UNSEEN;
            self.last_vt[i] = 0;
            i += 1;
        }
    }

    /// One market text frame, received at `ts_ns` (engine clock).
    /// Actions it causes (a settlement's unsubscribe) are queued: call
    /// [`Self::perform`] once the frame's borrow has ended.
    pub fn on_market_text<C: Capture>(
        &mut self,
        p: &[u8],
        ts_ns: NsTs,
        wall_ns: u64,
        out: &mut Pm15Out<'_, C>,
    ) {
        out.capture.raw_frame(ts_ns, p);
        out.status.add_msgs(1);
        if p == b"PONG" {
            self.reassert_touches(ts_ns, out);
            return;
        }
        match kind_of(p) {
            Kind::Book => self.on_book(p, ts_ns, out),
            Kind::PriceChange => self.on_price_change(p, ts_ns, out),
            Kind::TickSize => self.on_tick_size(p, ts_ns, out),
            Kind::LastTrade => self.on_last_trade(p, ts_ns, out),
            Kind::Resolved => self.on_resolved(p, ts_ns, wall_ns, out),
            Kind::Other => {}
        }
    }

    /// One RTDS text frame, received at `ts_ns`.
    pub fn on_rtds_text<C: Capture>(&mut self, p: &[u8], ts_ns: NsTs, out: &mut Pm15Out<'_, C>) {
        let mut px = ChainlinkPx::default();
        match parse_rtds(p, &mut px) {
            RtdsFrame::Other => {}
            RtdsFrame::Malformed => {
                out.counters.inc(Pm15Counter::RtdsRejects);
                out.capture.parse_reject(ts_ns, p);
            }
            RtdsFrame::Report => {
                let Some(f) = self.fams.family_of_asset(px.asset(p)) else {
                    return;
                };
                let sec = px.ts_ms / 1_000;
                if sec <= self.last_mark_s[f] {
                    out.counters.inc(Pm15Counter::MarksDuplicate);
                    return;
                }
                self.last_mark_s[f] = sec;
                if self.rtds_clock.judge(px.ts_ms, ts_ns).stale {
                    out.counters.inc(Pm15Counter::MarksStale);
                }
                out.counters.inc(Pm15Counter::Marks);
                let ev = ChannelEvent::new(
                    ts_ns,
                    VenueId::Polymarket,
                    ChannelId::Mark,
                    pm_mark_sym(f),
                    sec,
                    px.ts_ms,
                    px.px_1e6,
                    px.px_1e12,
                );
                emit_event(&ev, out);
            }
        }
    }

    /// Perform the queued actions (module docs): the roll events, and the
    /// subscribe / unsubscribe frames when the market session is open.
    pub fn perform<C: Capture>(
        &mut self,
        ts_ns: NsTs,
        out: &mut Pm15Out<'_, C>,
        wire: &mut impl Pm15Wire,
    ) {
        let mut k = 0usize;
        while k < self.acts.as_slice().len() {
            let act = self.acts.as_slice()[k];
            k += 1;
            match act {
                PmAction::Subscribe(s) => {
                    let s = s as usize;
                    let mut leg = 0usize;
                    while leg < 2 {
                        let i = 2 * s + leg;
                        self.ladders[i].clear();
                        self.last[i] = TOUCH_UNSEEN;
                        self.seq[i].reset_clock();
                        self.last_vt[i] = 0;
                        leg += 1;
                    }
                    if wire.is_open() {
                        let b = self.fams.slot(s);
                        let _ = wire.queue(&[
                            DYN_HEAD,
                            b.spec.up.as_bytes(),
                            DYN_MID,
                            b.spec.down.as_bytes(),
                            DYN_SUB,
                        ]);
                    }
                }
                PmAction::Unsubscribe(up, down) => {
                    if wire.is_open() {
                        let _ = wire.queue(&[
                            DYN_HEAD,
                            up.as_bytes(),
                            DYN_MID,
                            down.as_bytes(),
                            DYN_UNSUB,
                        ]);
                    }
                }
                PmAction::Created(s) => self.created(s as usize, ts_ns, out),
                PmAction::Settled {
                    slot,
                    instance,
                    window,
                    winner,
                    venue_ms,
                } => {
                    let s = slot as usize;
                    let (f, pair) = (s / PM_PAIRS, s % PM_PAIRS);
                    let ev = ChannelEvent::new(
                        ts_ns,
                        VenueId::Polymarket,
                        ChannelId::InstrumentRoll,
                        pm_rolling_sym(f, pair, PM_LEG_UP),
                        pack_roll_seq(instance, PM_TWAP_S, s, true),
                        venue_ms,
                        winner,
                        pm_window_end_ns(window) as i64,
                    );
                    emit_event(&ev, out);
                    out.counters.inc(if winner == 0 {
                        Pm15Counter::RollsUnknown
                    } else {
                        Pm15Counter::RollsSettled
                    });
                }
            }
        }
        self.acts.clear();
        out.counters
            .slots_bound
            .store(self.bound() as u64, Ordering::Relaxed);
    }

    /// CREATED, the two legs' order law, and the touches as they stand.
    fn created<C: Capture>(&mut self, s: usize, ts_ns: NsTs, out: &mut Pm15Out<'_, C>) {
        let (instance, window) = (self.fams.slot(s).instance, self.fams.slot(s).spec.window);
        let (f, pair) = (s / PM_PAIRS, s % PM_PAIRS);
        let ev = ChannelEvent::new(
            ts_ns,
            VenueId::Polymarket,
            ChannelId::InstrumentRoll,
            pm_rolling_sym(f, pair, PM_LEG_UP),
            pack_roll_seq(instance, PM_TWAP_S, s, false),
            0,
            0,
            pm_window_end_ns(window) as i64,
        );
        emit_event(&ev, out);
        out.counters.inc(Pm15Counter::RollsCreated);
        let mut leg = 0u32;
        while leg < 2 {
            self.emit_order_law(s, leg, 0, ts_ns, out);
            let i = 2 * s + leg as usize;
            self.last[i] = TOUCH_UNSEEN;
            if self.ladders[i].is_ready() {
                self.emit_touch(s, leg, 0, ts_ns, out);
            }
            leg += 1;
        }
    }

    fn emit_order_law<C: Capture>(
        &self,
        s: usize,
        leg: u32,
        venue_ms: u64,
        ts_ns: NsTs,
        out: &mut Pm15Out<'_, C>,
    ) {
        let b = self.fams.slot(s);
        let ev = ChannelEvent::new(
            ts_ns,
            VenueId::Polymarket,
            ChannelId::OutcomeMeta,
            pm_rolling_sym(s / PM_PAIRS, s % PM_PAIRS, leg),
            u64::from(b.instance),
            venue_ms,
            b.tick_1e6[(leg & 1) as usize],
            b.spec.min_size_1e6,
        );
        emit_event(&ev, out);
    }

    /// Emit leg `(s, leg)`'s touch if it changed since the last one.
    fn emit_touch<C: Capture>(
        &mut self,
        s: usize,
        leg: u32,
        venue_ms: u64,
        ts_ns: NsTs,
        out: &mut Pm15Out<'_, C>,
    ) {
        let i = (2 * s + leg as usize) % PM15_LEGS;
        let t = self.ladders[i].touch();
        if t == self.last[i] {
            return;
        }
        self.last[i] = t;
        self.last_vt[i] = self.last_vt[i].max(venue_ms);
        self.push_touch(i, t, venue_ms, 0, ts_ns, out);
    }

    /// The market socket answered our `PING`: it delivers in order, so every
    /// frame sent before the `PONG` has been read and each touch emitted is
    /// still the venue's — re-assert it, so a quiet book is not mistaken for
    /// a stale one (`strategy_pm15::TOUCH_FRESH_NS`). At most one tick per
    /// leg per `PM15_MARKET_PING_NS`.
    fn reassert_touches<C: Capture>(&mut self, ts_ns: NsTs, out: &mut Pm15Out<'_, C>) {
        let mut i = 0usize;
        while i < PM15_LEGS {
            if self.last[i] != TOUCH_UNSEEN
                && self.ladders[i].is_ready()
                && self.fams.slot(i / 2).ticks()
            {
                let t = self.last[i];
                // The key of the touch re-asserted: the sequencer's newest,
                // so it takes the next ordinal (a same-key duplicate does).
                self.push_touch(i, t, self.last_vt[i], 0, ts_ns, out);
                out.counters.inc(Pm15Counter::TouchesReasserted);
            }
            i += 1;
        }
    }

    /// Stamp, sequence, capture and push leg `i`'s touch `t`; `flags` is
    /// OR-ed with the clock's staleness verdict.
    fn push_touch<C: Capture>(
        &mut self,
        i: usize,
        t: Touch15,
        venue_ms: u64,
        flags: u8,
        ts_ns: NsTs,
        out: &mut Pm15Out<'_, C>,
    ) {
        let (s, leg) = (i / 2, (i % 2) as u32);
        let seq = self.seq[i].next(venue_ms);
        out.status.note_arrival_seq(seq);
        let judged = self.market_clock.judge(venue_ms, ts_ns);
        if judged.stale {
            out.status.inc_stale_ticks();
        }
        out.status
            .set_feed_delay_ema_ms(self.market_clock.delay_ema_ms());
        out.status.add_ticks(1);
        let tick = Tick::new_stamped(
            ts_ns,
            VenueId::Polymarket,
            pm_rolling_sym(s / PM_PAIRS, s % PM_PAIRS, leg),
            seq,
            Price::from_raw(t.bid_px),
            Qty::from_raw(t.bid_sz),
            Price::from_raw(t.ask_px),
            Qty::from_raw(t.ask_sz),
            venue_ms,
            flags | ((judged.stale as u8) * TICK_FLAG_STALE),
        );
        // §6.5: capture before the push.
        out.capture.tick(&tick);
        if !out.tick.try_push_ref(&tick) {
            out.status.inc_ring_drops();
        }
    }

    fn on_book<C: Capture>(&mut self, p: &[u8], ts_ns: NsTs, out: &mut Pm15Out<'_, C>) {
        const MARKER: &[u8] = b"{\"market\":\"";
        let mut at = 0usize;
        while let Some(off) = memchr::memmem::find(&p[at..], MARKER) {
            let start = at + off;
            let end = match memchr::memmem::find(&p[start + MARKER.len()..], MARKER) {
                Some(o) => start + MARKER.len() + o,
                None => p.len(),
            };
            let ev = &p[start..end];
            at = end;
            let Some((s, leg)) = asset_of(ev).and_then(|id| self.fams.find_token(id)) else {
                continue;
            };
            let i = 2 * s + leg as usize;
            if self.ladders[i].load_book(ev).is_err() {
                out.counters.inc(Pm15Counter::BookRejects);
                out.status.inc_parse_errors();
                out.capture.parse_reject(ts_ns, ev);
                continue;
            }
            if self.fams.slot(s).ticks() {
                let vt = scan_venue_time_ms(ev).unwrap_or(0);
                self.emit_touch(s, leg, vt, ts_ns, out);
            }
        }
    }

    fn on_price_change<C: Capture>(&mut self, p: &[u8], ts_ns: NsTs, out: &mut Pm15Out<'_, C>) {
        const ROW: &[u8] = b"\"asset_id\":\"";
        let vt = scan_venue_time_ms(p).unwrap_or(0);
        // The legs this event touched and their final venue touch.
        let mut touched = [(usize::MAX, 0i64, 0i64); 4];
        let mut n_t = 0usize;
        let mut at = 0usize;
        while let Some(off) = memchr::memmem::find(&p[at..], ROW) {
            let start = at + off;
            let end = match memchr::memmem::find(&p[start + ROW.len()..], ROW) {
                Some(o) => start + ROW.len() + o,
                None => p.len(),
            };
            let row = &p[start..end];
            at = end;
            let Some((s, leg)) = asset_of(row).and_then(|id| self.fams.find_token(id)) else {
                continue;
            };
            let i = 2 * s + leg as usize;
            let Some(r) = parse_change_row(row) else {
                out.counters.inc(Pm15Counter::RowRejects);
                out.status.inc_parse_errors();
                out.capture.parse_reject(ts_ns, row);
                continue;
            };
            if !self.ladders[i].is_ready() {
                // Before its snapshot a ladder has nothing to move.
                continue;
            }
            if self.ladders[i].set_level(r.is_bid, r.px, r.size).is_err() {
                out.counters.inc(Pm15Counter::RowRejects);
                continue;
            }
            let mut k = 0usize;
            while k < n_t && touched[k].0 != i {
                k += 1;
            }
            if k == n_t {
                if n_t == touched.len() {
                    // More legs in one event than a market has: settle the
                    // oldest now rather than drop one.
                    self.finish_touch(touched[0], vt, ts_ns, out);
                    touched[0] = touched[n_t - 1];
                    k = n_t - 1;
                } else {
                    n_t += 1;
                }
            }
            touched[k] = (i, r.best_bid, r.best_ask);
        }
        let mut k = 0usize;
        while k < n_t {
            self.finish_touch(touched[k], vt, ts_ns, out);
            k += 1;
        }
    }

    /// Apply a leg's final venue touch of an event and emit it.
    fn finish_touch<C: Capture>(
        &mut self,
        t: (usize, i64, i64),
        vt: u64,
        ts_ns: NsTs,
        out: &mut Pm15Out<'_, C>,
    ) {
        let (i, bb, ba) = t;
        if self.ladders[i % PM15_LEGS].apply_touch(bb, ba).is_err() {
            out.counters.inc(Pm15Counter::TouchOffGrid);
            return;
        }
        let (s, leg) = (i / 2, (i % 2) as u32);
        if self.fams.slot(s).ticks() {
            self.emit_touch(s, leg, vt, ts_ns, out);
        }
    }

    fn on_tick_size<C: Capture>(&mut self, p: &[u8], ts_ns: NsTs, out: &mut Pm15Out<'_, C>) {
        let Some((s, leg)) = asset_of(p).and_then(|id| self.fams.find_token(id)) else {
            return;
        };
        let Some(tick) = quoted_price(p, b"\"new_tick_size\":") else {
            out.status.inc_parse_errors();
            out.capture.parse_reject(ts_ns, p);
            return;
        };
        let b = self.fams.slot(s);
        if b.tick_1e6[(leg & 1) as usize] == tick {
            return;
        }
        // The ladder holds the 0.01 and 0.001 grids only.
        let broken = tick != 10_000 && tick != 1_000;
        let was_broken = b.broken != 0;
        self.fams.set_tick(s, leg, tick, broken);
        out.counters.inc(Pm15Counter::TickChanges);
        if broken && !was_broken {
            out.counters.inc(Pm15Counter::SlotsBroken);
        }
        if matches!(
            self.fams.slot(s).state,
            crate::rolling::PmSlotState::Live | crate::rolling::PmSlotState::Ended
        ) {
            let vt = scan_venue_time_ms(p).unwrap_or(0);
            self.emit_order_law(s, leg, vt, ts_ns, out);
        }
    }

    fn on_last_trade<C: Capture>(&mut self, p: &[u8], ts_ns: NsTs, out: &mut Pm15Out<'_, C>) {
        let Some((s, leg)) = asset_of(p).and_then(|id| self.fams.find_token(id)) else {
            return;
        };
        if !self.fams.slot(s).ticks() {
            return;
        }
        let (Some(px), Some(sz)) = (
            quoted_price(p, b"\"price\":"),
            quoted_price(p, b"\"size\":"),
        ) else {
            out.status.inc_parse_errors();
            out.capture.parse_reject(ts_ns, p);
            return;
        };
        let side = if memchr::memmem::find(p, b"\"side\":\"BUY\"").is_some() {
            1
        } else {
            2
        };
        out.counters.inc(Pm15Counter::Trades);
        let ev = ChannelEvent::new(
            ts_ns,
            VenueId::Polymarket,
            ChannelId::Trade,
            pm_rolling_sym(s / PM_PAIRS, s % PM_PAIRS, leg),
            side,
            scan_venue_time_ms(p).unwrap_or(0),
            px,
            sz,
        );
        emit_event(&ev, out);
    }

    fn on_resolved<C: Capture>(
        &mut self,
        p: &[u8],
        ts_ns: NsTs,
        wall_ns: u64,
        out: &mut Pm15Out<'_, C>,
    ) {
        let (Some(win), Some(word)) = (
            quoted(p, b"\"winning_asset_id\":"),
            quoted(p, b"\"winning_outcome\":"),
        ) else {
            out.status.inc_parse_errors();
            out.capture.parse_reject(ts_ns, p);
            return;
        };
        let vt = scan_venue_time_ms(p).unwrap_or(0);
        match self
            .fams
            .on_resolved(win, word, vt, wall_ns, &mut self.acts)
        {
            PmResolved::Settled => {}
            PmResolved::Mismatch => {}
            PmResolved::Unknown => out.counters.inc(Pm15Counter::ResolvedForeign),
        }
    }
}

/// The first `event_type` of a market frame.
fn kind_of(p: &[u8]) -> Kind {
    let Some(v) = quoted(p, b"\"event_type\":") else {
        return Kind::Other;
    };
    match v {
        b"book" => Kind::Book,
        b"price_change" => Kind::PriceChange,
        b"tick_size_change" => Kind::TickSize,
        b"last_trade_price" => Kind::LastTrade,
        b"market_resolved" => Kind::Resolved,
        _ => Kind::Other,
    }
}

/// A quoted string field's value (`"key":"value"`, a space after the
/// colon tolerated), a span of `p`.
fn quoted<'a>(p: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let at = core_parse::skip_ws(p, core_parse::find_field(p, key)?);
    if p.get(at) != Some(&b'"') {
        return None;
    }
    let end = at + 1 + memchr::memchr(b'"', p.get(at + 1..)?)?;
    p.get(at + 1..end)
}

/// The `asset_id` of an event or a row.
#[inline]
fn asset_of(p: &[u8]) -> Option<&[u8]> {
    quoted(p, b"\"asset_id\":")
}

/// One event: the capture always, the lane when its channel is masked in.
#[inline]
fn emit_event<C: Capture>(ev: &ChannelEvent, out: &mut Pm15Out<'_, C>) {
    out.capture.event(ev);
    if out.event_mask & (1u16 << (ev.channel & 15)) != 0 && !out.event.try_push_ref(ev) {
        out.status.inc_event_ring_drops();
    }
}

// ---------------------------------------------------------------
// The loop
// ---------------------------------------------------------------

/// The two sockets (built at boot by the spawner: `WsConn::new` resolves).
pub struct Pm15Sockets {
    /// The market socket, token [`PM15_MARKET_TOKEN`].
    pub market: WsConn,
    /// The RTDS socket, token [`PM15_RTDS_TOKEN`].
    pub rtds: WsConn,
}

/// One session's dial schedule.
struct Dial {
    backoff: Backoff,
    next_ns: u64,
    ping_ns: u64,
}

impl Dial {
    fn failed(&mut self, now: u64) {
        self.next_ns = now.saturating_add(self.backoff.next_delay_ns());
    }
}

/// Run the PM15 ingress until `stop` (module docs). Returns on `stop`, or
/// on a poll failure (the spawner logs it; the thread then ends).
///
/// # Errors
/// The `mio` poll could not be created or polled.
#[allow(clippy::too_many_arguments)]
pub fn run_pm15<C: Capture>(
    driver: &mut Pm15Driver,
    socks: &mut Pm15Sockets,
    specs: &mut Consumer<PmWindowSpec, PM15_SPEC_RING>,
    out: &mut Pm15Out<'_, C>,
    wall: WallAnchor,
    seed: u64,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut poll = mio::Poll::new()?;
    let mut events = mio::Events::with_capacity(64);
    let mut md = Dial {
        backoff: Backoff::default_for_ingress(seed),
        next_ns: 0,
        ping_ns: 0,
    };
    let mut rd = Dial {
        backoff: Backoff::default_for_ingress(seed ^ 0x5a5a),
        next_ns: 0,
        ping_ns: 0,
    };
    out.status.set_state(IngressState::Connecting);
    while !stop.load(Ordering::Relaxed) {
        poll.poll(&mut events, Some(POLL_TIMEOUT))?;
        for ev in events.iter() {
            let now = now_ns();
            if ev.token() == PM15_MARKET_TOKEN {
                match socks.market.on_event(ev, poll.registry(), now) {
                    WsProgress::Opened => {
                        md.backoff.reset();
                        out.status.set_state(IngressState::Up);
                        if !driver.on_market_open(&mut socks.market) {
                            socks.market.close();
                            market_down(driver, &mut md, out, now, WsErr::Overflow);
                            continue;
                        }
                        md.ping_ns = now.saturating_add(PM15_MARKET_PING_NS);
                        drain_market(driver, socks, out, &mut md, wall);
                    }
                    WsProgress::Readable => drain_market(driver, socks, out, &mut md, wall),
                    WsProgress::Failed(e) => market_down(driver, &mut md, out, now, e),
                    WsProgress::Waiting => {}
                }
            } else if ev.token() == PM15_RTDS_TOKEN {
                match socks.rtds.on_event(ev, poll.registry(), now) {
                    WsProgress::Opened => {
                        rd.backoff.reset();
                        if socks.rtds.queue_text(&[RTDS_SUBSCRIBE]).is_err() {
                            socks.rtds.close();
                            rtds_down(driver, &mut rd, out, now, WsErr::Overflow);
                            continue;
                        }
                        rd.ping_ns = now.saturating_add(RTDS_PING_NS);
                        drain_rtds(driver, socks, out, &mut rd);
                    }
                    WsProgress::Readable => drain_rtds(driver, socks, out, &mut rd),
                    WsProgress::Failed(e) => rtds_down(driver, &mut rd, out, now, e),
                    WsProgress::Waiting => {}
                }
            }
        }
        let now = now_ns();
        let wall_now = wall.wall_of(now);
        // The windows discovery found.
        while let Some(spec) = specs.try_pop_ref() {
            // COPY: the spec (≈ 260 B) off the ring — cold, once per window.
            let sp = *spec;
            drop(spec);
            driver.offer(&sp, wall_now, now, out, &mut socks.market);
        }
        driver.on_clock(wall_now, now, out, &mut socks.market);
        // Dials: the market once a window is bound, RTDS at once.
        if socks.market.is_down() && driver.bound() > 0 && now >= md.next_ns {
            out.status.set_state(IngressState::Connecting);
            if let Err(e) = socks.market.connect(poll.registry(), now) {
                market_down(driver, &mut md, out, now, e);
            }
        }
        if socks.rtds.is_down() && !driver.families().is_empty() && now >= rd.next_ns {
            if let Err(e) = socks.rtds.connect(poll.registry(), now) {
                rtds_down(driver, &mut rd, out, now, e);
            }
        }
        // Clocks and keepalives.
        if let WsProgress::Failed(e) = socks.market.on_tick(now) {
            market_down(driver, &mut md, out, now, e);
        }
        if let WsProgress::Failed(e) = socks.rtds.on_tick(now) {
            rtds_down(driver, &mut rd, out, now, e);
        }
        if socks.market.is_open() && now >= md.ping_ns {
            md.ping_ns = now.saturating_add(PM15_MARKET_PING_NS);
            if let Err(e) = socks.market.queue_text(&[b"PING"]) {
                socks.market.close();
                market_down(driver, &mut md, out, now, e);
            }
        }
        if socks.rtds.is_open() && now >= rd.ping_ns {
            rd.ping_ns = now.saturating_add(RTDS_PING_NS);
            if let Err(e) = socks.rtds.queue_text(&[RTDS_PING]) {
                socks.rtds.close();
                rtds_down(driver, &mut rd, out, now, e);
            }
        }
        if socks.market.wants_flush() {
            if let Err(e) = socks.market.flush(poll.registry()) {
                market_down(driver, &mut md, out, now, e);
            }
        }
        if socks.rtds.wants_flush() {
            if let Err(e) = socks.rtds.flush(poll.registry()) {
                rtds_down(driver, &mut rd, out, now, e);
            }
        }
        out.capture.maybe_flush(now);
    }
    socks.market.close();
    socks.rtds.close();
    out.status.set_state(IngressState::Down);
    Ok(())
}

fn market_down<C: Capture>(
    driver: &mut Pm15Driver,
    d: &mut Dial,
    out: &mut Pm15Out<'_, C>,
    now: u64,
    e: WsErr,
) {
    out.counters.note_ws_err(0, e);
    driver.on_market_down(now, out);
    d.failed(now);
    out.status.inc_reconnects();
    out.status.set_state(IngressState::Backoff);
    out.counters.inc(Pm15Counter::MarketReconnects);
}

fn rtds_down<C: Capture>(
    driver: &mut Pm15Driver,
    d: &mut Dial,
    out: &mut Pm15Out<'_, C>,
    now: u64,
    e: WsErr,
) {
    out.counters.note_ws_err(1, e);
    driver.on_rtds_down();
    d.failed(now);
    out.counters.inc(Pm15Counter::RtdsReconnects);
}

/// Every buffered market frame, then the actions they caused.
fn drain_market<C: Capture>(
    driver: &mut Pm15Driver,
    socks: &mut Pm15Sockets,
    out: &mut Pm15Out<'_, C>,
    d: &mut Dial,
    wall: WallAnchor,
) {
    loop {
        match socks.market.next_frame() {
            WsNext::Text(span) => {
                let now = now_ns();
                let p = socks.market.payload(span);
                out.status.touch_activity(now);
                out.status.add_bytes(p.len() as u64);
                driver.on_market_text(p, now, wall.wall_of(now), out);
                driver.perform(now, out, &mut socks.market);
            }
            WsNext::Binary(_) => {}
            WsNext::Idle => return,
            WsNext::Failed(e) => {
                market_down(driver, d, out, now_ns(), e);
                return;
            }
        }
    }
}

/// Every buffered RTDS frame.
fn drain_rtds<C: Capture>(
    driver: &mut Pm15Driver,
    socks: &mut Pm15Sockets,
    out: &mut Pm15Out<'_, C>,
    d: &mut Dial,
) {
    loop {
        match socks.rtds.next_frame() {
            WsNext::Text(span) => {
                let now = now_ns();
                let p = socks.rtds.payload(span);
                driver.on_rtds_text(p, now, out);
            }
            WsNext::Binary(_) => {}
            WsNext::Idle => return,
            WsNext::Failed(e) => {
                rtds_down(driver, d, out, now_ns(), e);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
