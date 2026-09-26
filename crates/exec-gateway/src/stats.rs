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

/// Written by the ENGINE thread only.
#[repr(C, align(128))]
struct EngineWritten {
    cmd_ring_full: AtomicU64,
    cmds_pushed: AtomicU64,
}

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
    ws_up: AtomicU64,
}

/// Shared stats of one gateway (one venue account): built once at boot,
/// held by both ends of the duplex behind an `Arc`.
#[repr(C, align(128))]
pub struct GatewayStats {
    eng: EngineWritten,
    halt: HaltWord,
    gw: GatewayWritten,
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
pub const GATEWAY_STATS_SIZE: usize = 66_432;

const _: () = assert!(core::mem::align_of::<GatewayStats>() == 128);
const _: () = assert!(core::mem::size_of::<EngineWritten>() == 128);
const _: () = assert!(core::mem::size_of::<HaltWord>() == 128);
const _: () = assert!(core::mem::size_of::<GatewayWritten>() == 128);
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
                ws_up: z(),
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
        if self.gw.ws_up.load(Ordering::Relaxed) != up {
            self.gw.ws_up.store(up, Ordering::Relaxed);
        }
    }

    // --- readers (either thread, /metrics) ---------------------------

    /// The halt reason, [`HALT_NONE`] while running.
    #[inline]
    pub fn halt_reason(&self) -> u64 {
        self.halt.reason.load(Ordering::Relaxed)
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
            g.ws_up.load(Ordering::Relaxed),
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
