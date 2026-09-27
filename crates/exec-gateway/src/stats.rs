// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The one block of state both threads of an arm may touch: counters,
//! gauges, the halt word and the latency trackers.
//!
//! Every field is an atomic written by exactly ONE side (single writer)
//! and read by the other and by the `/metrics` publisher, with relaxed
//! ordering — observations, never synchronisation (the rings carry the
//! data). Each writer's fields sit in their own 128 B granule (the Apple
//! M-series cache line; x86's adjacent-line prefetcher pairs 64 B
//! lines), and the halt word has a granule to itself: the engine reads it
//! before every order, so no counter the gateway bumps may share its line.

use core::sync::atomic::{AtomicU64, Ordering};

use core_latency::LatencyTracker;

/// Rows of every latency tracker: 0 ns … 2^32 − 1 ns (≈ 4.3 s). A
/// longer sample is clamped into the top bucket (the tracker's own
/// column math wraps above it); [`Lat::max_ns`] keeps the exact value.
pub const LAT_ROWS: usize = 32;
const LAT_CLAMP_NS: u64 = (1u64 << LAT_ROWS) - 1;

// The halt reasons — ONE registry for every gateway built on this kit.
// The kit owns 1..=15; a venue gateway numbers its own from 16.

/// Not halted.
pub const HALT_NONE: u64 = 0;
/// The event ring was full: an event (possibly a fill) was lost.
pub const HALT_EVENT_RING_FULL: u64 = 1;
/// A deadline could not be armed (the timer queue is full): a sizing bug.
pub const HALT_TIMER_FULL: u64 = 2;
/// The in-flight table refused a post id (a stuck post held its slot).
pub const HALT_PENDING_STUCK: u64 = 3;
/// The first reason a venue gateway may use.
pub const HALT_VENUE_BASE: u64 = 16;

/// One latency series: the log-linear tracker plus the exact maximum.
#[repr(C, align(128))]
pub struct Lat {
    hist: LatencyTracker<LAT_ROWS>,
    max_ns: AtomicU64,
}

impl Lat {
    fn new() -> Self {
        Self {
            hist: LatencyTracker::new(),
            max_ns: AtomicU64::new(0),
        }
    }

    /// Record one sample (ns). Relaxed atomics; 0 B/op.
    #[inline]
    pub fn record(&self, ns: u64) {
        self.hist.record(ns.min(LAT_CLAMP_NS));
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }

    /// Samples recorded.
    #[inline]
    pub fn count(&self) -> u64 {
        self.hist.count()
    }

    /// The `p` quantile's bucket bound (ns), `p` in `[0, 1]`; 0 when
    /// empty. O(2 048) loads: a `/metrics` read, never a loop's.
    #[inline]
    pub fn quantile_ns(&self, p: f64) -> u64 {
        self.hist.percentile(p)
    }

    /// The largest sample recorded (ns).
    #[inline]
    pub fn max_ns(&self) -> u64 {
        self.max_ns.load(Ordering::Relaxed)
    }
}

/// Written by the ENGINE thread only, on every push.
#[repr(C, align(128))]
struct EngineWritten {
    cmd_ring_full: AtomicU64,
    cmds_pushed: AtomicU64,
}

/// Written by the ENGINE thread only, rarely: the gateway reads it every
/// turn, so it must not share a line with the per-push counters.
#[repr(C, align(128))]
struct EngineRare {
    /// The last cancel-all (sweep) request's sequence number.
    sweep_req: AtomicU64,
}

/// A sweep's state (XH4d, XG-1: a cancel-all never waits on ring room):
/// the engine bumps the request word, the gateway publishes the state of
/// the request it last took up. The values are `CancelAllState`'s.
/// Nothing of the gateway's slots is resting — the venue said so.
pub const SWEEP_CLEAR: u64 = 0;
/// The sweep is under way.
pub const SWEEP_WORKING: u64 = 1;
/// The sweep spent its rounds and the venue never read clear: ask again.
pub const SWEEP_STRANDED: u64 = 2;

/// XH4e — the congestion word ([`GatewayStats::congestion`]): the rates
/// have room.
pub const CONGESTION_NONE: u64 = 0;
/// Busy: a rate window over 60 % of its share, most of the in-flight
/// bound used, or the venue's answers slow.
pub const CONGESTION_BUSY: u64 = 1;
/// Throttled: a rate window at 80 % of its share — growth is refused
/// unsent; cancels, reads and pings still go.
pub const CONGESTION_THROTTLED: u64 = 2;
/// The venue answered a rate-limit error in the last 10 s.
pub const CONGESTION_RATE_LIMITED: u64 = 3;

/// The halt word: written by the gateway (at most once per arm), read by
/// the engine before every order.
#[repr(C, align(128))]
struct HaltWord {
    reason: AtomicU64,
}

/// Written by the GATEWAY thread only.
#[repr(C, align(128))]
struct GatewayWritten {
    evt_ring_full: AtomicU64,
    cmds_popped: AtomicU64,
    actions_sent: AtomicU64,
    replies: AtomicU64,
    replies_unmatched: AtomicU64,
    post_timeouts: AtomicU64,
    rest_fallback_posts: AtomicU64,
    reconnects: AtomicU64,
    in_flight: AtomicU64,
}

/// Written by the GATEWAY thread only, rarely: the sweep words when a
/// request changes, the heartbeat and (XH4e) the congestion word at most
/// ten times a second, the sweep's cancel count once a sweep round. The
/// engine reads it on every idle pass and halt poll, so it must not share
/// a line with the per-post counters.
#[repr(C, align(128))]
struct GatewayLive {
    ws_up: AtomicU64,
    /// The gateway's last heartbeat, engine-clock ns.
    heartbeat_ns: AtomicU64,
    /// The sweep request the state below is for.
    sweep_seq: AtomicU64,
    /// [`SWEEP_CLEAR`] / [`SWEEP_WORKING`] / [`SWEEP_STRANDED`].
    sweep_state: AtomicU64,
    /// XH4e: `CONGESTION_*` — how close the gateway runs to its share of
    /// the venue's rate limits (advisory: the gateway enforces the
    /// throttle itself); published with the heartbeat.
    congestion: AtomicU64,
    /// XH4e: cancels the gateway's own sweeps sent, ever — address
    /// requests the engine charges to its budget by the delta; one add a
    /// sweep round.
    sweep_cancels: AtomicU64,
}

/// Shared stats of one gateway (one venue account): built once at boot,
/// held by both ends of the duplex behind an `Arc`.
#[repr(C, align(128))]
pub struct GatewayStats {
    eng: EngineWritten,
    eng_rare: EngineRare,
    halt: HaltWord,
    gw: GatewayWritten,
    live: GatewayLive,
    /// Command push (engine) → the frame carrying it written to the socket.
    pub push_to_write: Lat,
    /// Action written → the venue's reply to that post.
    pub post_rtt: Lat,
    /// Cancel written → the venue's reply to that cancel.
    pub cancel_rtt: Lat,
    /// A fill's venue time → its local receipt.
    pub fill_venue_to_local: Lat,
}

/// The block's size — pinned: it is built once per arm at boot, on the
/// booting thread's stack before it moves into its `Arc` (≈ 65 KiB; the
/// kit has no `unsafe` to build it in place, and a boot thread's stack is
/// 8 MiB). A size change is reviewed here.
pub const GATEWAY_STATS_SIZE: usize = 66_688;

const _: () = assert!(core::mem::align_of::<GatewayStats>() == 128);
const _: () = assert!(core::mem::size_of::<EngineWritten>() == 128);
const _: () = assert!(core::mem::size_of::<EngineRare>() == 128);
const _: () = assert!(core::mem::size_of::<HaltWord>() == 128);
const _: () = assert!(core::mem::size_of::<GatewayWritten>() == 128);
const _: () = assert!(core::mem::size_of::<GatewayLive>() == 128);
const _: () = assert!(core::mem::size_of::<GatewayStats>() == GATEWAY_STATS_SIZE);

impl GatewayStats {
    /// Boot-time constructor (see [`GATEWAY_STATS_SIZE`]).
    pub(crate) fn new() -> Self {
        let z = || AtomicU64::new(0);
        Self {
            eng: EngineWritten {
                cmd_ring_full: z(),
                cmds_pushed: z(),
            },
            eng_rare: EngineRare { sweep_req: z() },
            halt: HaltWord { reason: z() },
            gw: GatewayWritten {
                evt_ring_full: z(),
                cmds_popped: z(),
                actions_sent: z(),
                replies: z(),
                replies_unmatched: z(),
                post_timeouts: z(),
                rest_fallback_posts: z(),
                reconnects: z(),
                in_flight: z(),
            },
            live: GatewayLive {
                ws_up: z(),
                heartbeat_ns: z(),
                sweep_seq: z(),
                sweep_state: z(),
                congestion: z(),
                sweep_cancels: z(),
            },
            push_to_write: Lat::new(),
            post_rtt: Lat::new(),
            cancel_rtt: Lat::new(),
            fill_venue_to_local: Lat::new(),
        }
    }

    // --- engine-written ---------------------------------------------

    #[inline]
    pub(crate) fn note_pushed(&self) {
        self.eng.cmds_pushed.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn note_cmd_ring_full(&self) {
        self.eng.cmd_ring_full.fetch_add(1, Ordering::Relaxed);
    }

    /// Ask the gateway to sweep (cancel every order of its slots) —
    /// engine only; never waits on ring room. Returns the request's
    /// sequence number, which [`Self::sweep`] reports back.
    #[inline]
    pub fn request_sweep(&self) -> u64 {
        self.eng_rare.sweep_req.fetch_add(1, Ordering::Release) + 1
    }

    /// The last sweep request (gateway side).
    #[inline]
    pub fn sweep_requested(&self) -> u64 {
        self.eng_rare.sweep_req.load(Ordering::Acquire)
    }

    /// Publish the state of sweep request `seq` (gateway only): the state
    /// first, then the sequence it belongs to, so a reader that sees the
    /// sequence sees a state at least that new.
    #[inline]
    pub fn set_sweep(&self, seq: u64, state: u64) {
        self.live.sweep_state.store(state, Ordering::Relaxed);
        self.live.sweep_seq.store(seq, Ordering::Release);
    }

    /// `(the request the state is for, its state)`.
    #[inline]
    pub fn sweep(&self) -> (u64, u64) {
        let seq = self.live.sweep_seq.load(Ordering::Acquire);
        (seq, self.live.sweep_state.load(Ordering::Relaxed))
    }

    // --- gateway-written --------------------------------------------

    /// Set the halt word (gateway only). The first reason wins: a later
    /// fault never overwrites the one that stopped the arm.
    #[inline]
    pub fn halt(&self, reason: u64) {
        debug_assert!(reason != HALT_NONE, "a halt needs a reason");
        let _ = self.halt.reason.compare_exchange(
            HALT_NONE,
            reason,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    #[inline]
    pub(crate) fn note_evt_ring_full(&self) {
        self.gw.evt_ring_full.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn note_popped(&self) {
        self.gw.cmds_popped.fetch_add(1, Ordering::Relaxed);
    }

    /// One action (one signed post) written to the socket or REST.
    #[inline]
    pub fn note_action_sent(&self) {
        self.gw.actions_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// A reply matched to its post.
    #[inline]
    pub fn note_reply(&self) {
        self.gw.replies.fetch_add(1, Ordering::Relaxed);
    }

    /// A reply for a post the table does not hold (late or duplicate).
    #[inline]
    pub fn note_reply_unmatched(&self) {
        self.gw.replies_unmatched.fetch_add(1, Ordering::Relaxed);
    }

    /// A post whose reply never came inside its deadline.
    #[inline]
    pub fn note_post_timeout(&self) {
        self.gw.post_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    /// An action sent over the REST fallback (the WebSocket was down).
    #[inline]
    pub fn note_rest_fallback(&self) {
        self.gw.rest_fallback_posts.fetch_add(1, Ordering::Relaxed);
    }

    /// A connection swap or re-dial.
    #[inline]
    pub fn note_reconnect(&self) {
        self.gw.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    /// Gauges: posts in flight, and whether the WebSocket is up (0/1).
    /// Stored only on change (the gateway's own line stays exclusive).
    #[inline]
    pub fn set_gauges(&self, in_flight: u64, ws_up: bool) {
        let up = u64::from(ws_up);
        if self.gw.in_flight.load(Ordering::Relaxed) != in_flight {
            self.gw.in_flight.store(in_flight, Ordering::Relaxed);
        }
        if self.live.ws_up.load(Ordering::Relaxed) != up {
            self.live.ws_up.store(up, Ordering::Relaxed);
        }
    }

    /// The gateway's heartbeat (gateway only; the caller keeps it to its
    /// period): a turn happened at `now_ns`, engine clock.
    #[inline]
    pub fn beat(&self, now_ns: u64) {
        self.live.heartbeat_ns.store(now_ns, Ordering::Relaxed);
    }

    /// XH4e: publish the congestion level, `CONGESTION_*` (gateway only,
    /// with its heartbeat; stored on change).
    #[inline]
    pub fn set_congestion(&self, level: u64) {
        if self.live.congestion.load(Ordering::Relaxed) != level {
            self.live.congestion.store(level, Ordering::Relaxed);
        }
    }

    /// XH4e: a sweep round sent `n` cancels (gateway only; one add a
    /// round, none for an empty one).
    #[inline]
    pub fn note_sweep_cancels(&self, n: u64) {
        if n > 0 {
            self.live.sweep_cancels.fetch_add(n, Ordering::Relaxed);
        }
    }

    // --- readers (either thread, /metrics) ---------------------------

    /// The halt reason, [`HALT_NONE`] while running.
    #[inline]
    pub fn halt_reason(&self) -> u64 {
        self.halt.reason.load(Ordering::Relaxed)
    }

    /// Whether a WebSocket is up (the gauge).
    #[inline]
    pub fn ws_up(&self) -> bool {
        self.live.ws_up.load(Ordering::Relaxed) != 0
    }

    /// The gateway's last heartbeat, engine-clock ns (0 = none yet).
    #[inline]
    pub fn heartbeat_ns(&self) -> u64 {
        self.live.heartbeat_ns.load(Ordering::Relaxed)
    }

    /// XH4e: the congestion level, `CONGESTION_*`.
    #[inline]
    pub fn congestion(&self) -> u64 {
        self.live.congestion.load(Ordering::Relaxed)
    }

    /// XH4e: cancels the gateway's sweeps sent, ever.
    #[inline]
    pub fn sweep_cancels(&self) -> u64 {
        self.live.sweep_cancels.load(Ordering::Relaxed)
    }

    /// Commands the engine pushed / the ring refused (full).
    #[inline]
    pub fn cmds(&self) -> (u64, u64) {
        (
            self.eng.cmds_pushed.load(Ordering::Relaxed),
            self.eng.cmd_ring_full.load(Ordering::Relaxed),
        )
    }

    /// Commands the gateway popped; events it could not deliver (full).
    #[inline]
    pub fn gateway_ring(&self) -> (u64, u64) {
        (
            self.gw.cmds_popped.load(Ordering::Relaxed),
            self.gw.evt_ring_full.load(Ordering::Relaxed),
        )
    }

    /// `[actions_sent, replies, replies_unmatched, post_timeouts,
    /// rest_fallback_posts, reconnects, in_flight, ws_up]`.
    #[inline]
    pub fn io(&self) -> [u64; 8] {
        let g = &self.gw;
        [
            g.actions_sent.load(Ordering::Relaxed),
            g.replies.load(Ordering::Relaxed),
            g.replies_unmatched.load(Ordering::Relaxed),
            g.post_timeouts.load(Ordering::Relaxed),
            g.rest_fallback_posts.load(Ordering::Relaxed),
            g.reconnects.load(Ordering::Relaxed),
            g.in_flight.load(Ordering::Relaxed),
            self.live.ws_up.load(Ordering::Relaxed),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_halt_reason_wins() {
        let s = GatewayStats::new();
        assert_eq!(s.halt_reason(), HALT_NONE);
        s.halt(HALT_TIMER_FULL);
        s.halt(HALT_EVENT_RING_FULL);
        assert_eq!(
            s.halt_reason(),
            HALT_TIMER_FULL,
            "a later fault never overwrites the first"
        );
    }

    #[test]
    fn a_sample_past_the_top_row_lands_in_the_top_bucket_and_keeps_its_max() {
        let s = GatewayStats::new();
        s.post_rtt.record(1_000);
        s.post_rtt.record(250_000_000);
        s.post_rtt.record(4_800_000_000); // inside a 5 s timeout, past 2^32 ns
        s.post_rtt.record(9_000_000_000);
        assert_eq!(s.post_rtt.count(), 4);
        assert_eq!(s.post_rtt.max_ns(), 9_000_000_000);
        // Before the clamp the tracker's column wrapped a 4.8 s sample to
        // ≈ 2.4 s; clamped, the top quartile reads the top bucket.
        assert!(s.post_rtt.quantile_ns(0.75) >= LAT_CLAMP_NS - (LAT_CLAMP_NS >> 7));
        assert!(s.post_rtt.quantile_ns(0.5) >= 250_000_000);
        assert_eq!(s.cancel_rtt.count(), 0);
        assert_eq!(s.cancel_rtt.quantile_ns(0.99), 0, "an empty series reads 0");
    }

    #[test]
    fn each_writer_and_the_halt_word_own_their_granule() {
        let s = GatewayStats::new();
        let e = core::ptr::addr_of!(s.eng) as usize;
        let h = core::ptr::addr_of!(s.halt) as usize;
        let g = core::ptr::addr_of!(s.gw) as usize;
        for a in [e, h, g] {
            assert_eq!(a % 128, 0);
        }
        assert!(e.abs_diff(h) >= 128 && h.abs_diff(g) >= 128 && e.abs_diff(g) >= 128);
    }

    #[test]
    fn io_reads_every_gateway_counter_in_order() {
        let s = GatewayStats::new();
        s.note_action_sent();
        s.note_reply();
        s.note_reply_unmatched();
        s.note_post_timeout();
        s.note_rest_fallback();
        s.note_reconnect();
        s.set_gauges(5, true);
        assert_eq!(s.io(), [1, 1, 1, 1, 1, 1, 5, 1]);
        s.set_gauges(5, true);
        s.set_gauges(0, false);
        assert_eq!(s.io()[6..], [0, 0]);
    }
}
