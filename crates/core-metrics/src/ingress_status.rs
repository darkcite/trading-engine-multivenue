// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Per-ingress status slot — the D7 fix.
//!
//! One `IngressStatus` per ingress thread, allocated at boot and
//! shared via `Arc`. The **ingress thread is the only writer**
//! (single-writer doctrine); the cli metrics loop and the TUI read a
//! racy-but-monotonic snapshot each report period and mirror it into
//! registry gauges/counters. All accesses are `Relaxed` — this is
//! monitoring state, no synchronization is derived from it.
//!
//! Replaces the Phase-2 fiction `up = engine.iterations > 0` with the
//! real per-thread connection state, and carries the §6.4
//! loss-accounting counters (D4: `ring_drops` is incremented on every
//! failed `try_push`).

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

// ---------------------------------------------------------------
// Session-error diagnosability (capture-continuity outage 2026-08-27,
// remediation plan T1(a)): the run-loop's fatal sites historically
// discarded their cause — six days of venue outage logged only
// `res=Error`. Each ingress now records WHERE the session died
// (site code), the io::ErrorKind class, and the venue's numeric
// error code (when one exists) into its status slot; the cli venue
// loop reads-and-clears the triple on the SAME thread and names it
// on the `run-loop returned` log line. Zero allocation; plain
// relaxed atomics on the teardown path (never the tick path).
// ---------------------------------------------------------------

/// Session-error site: transport registration failed.
pub const ERR_SITE_REGISTER: u8 = 1;
/// Session-error site: `mio::Poll::poll` failed.
pub const ERR_SITE_POLL: u8 = 2;
/// Session-error site: transport pump (read/write readiness) failed.
pub const ERR_SITE_PUMP: u8 = 3;
/// Session-error site: the drive/drain step failed (protocol parse,
/// venue error event, subscribe echo, resync queueing, TX flush).
pub const ERR_SITE_DRIVE: u8 = 4;
/// Session-error site: keepalive probe queue/flush failed.
pub const ERR_SITE_KEEPALIVE: u8 = 5;
/// Session-error site: transport re-registration failed.
pub const ERR_SITE_REREGISTER: u8 = 6;
/// Inner session-error site: the venue answered with an explicit
/// error event/response (numeric code in `venue_code`).
pub const ERR_SITE_VENUE_ERROR: u8 = 7;
/// Inner session-error site: the subscribe echo/result was missing
/// configured channels (`venue_code` = count of missing channels).
pub const ERR_SITE_SUBSCRIBE_MISSING: u8 = 8;
/// Session-error site: the session failed to produce its FIRST
/// confirmed subscription within the establishment budget (WS2,
/// outage 2026-08-27 §5.3 — covers Connecting/AwaitingUpgrade
/// wedges AND zero-sub `Steady` sessions kept alive by pongs).
pub const ERR_SITE_ESTABLISH: u8 = 9;

/// Human name for a session-error site code (0 = "none").
#[inline]
pub const fn err_site_name(site: u8) -> &'static str {
    match site {
        0 => "none",
        ERR_SITE_REGISTER => "register",
        ERR_SITE_POLL => "poll",
        ERR_SITE_PUMP => "pump",
        ERR_SITE_DRIVE => "drive",
        ERR_SITE_KEEPALIVE => "keepalive",
        ERR_SITE_REREGISTER => "reregister",
        ERR_SITE_VENUE_ERROR => "venue-error",
        ERR_SITE_SUBSCRIBE_MISSING => "subscribe-missing",
        ERR_SITE_ESTABLISH => "establish-timeout",
        _ => "unknown",
    }
}

/// Stable compact code for an [`std::io::ErrorKind`] (0 = none,
/// 1 = other/unmapped). `ErrorKind` has no stable discriminant of
/// its own, hence this explicit table.
#[inline]
pub fn io_kind_code(k: std::io::ErrorKind) -> u8 {
    use std::io::ErrorKind as K;
    match k {
        K::ConnectionReset => 2,
        K::ConnectionAborted => 3,
        K::ConnectionRefused => 4,
        K::NotConnected => 5,
        K::BrokenPipe => 6,
        K::TimedOut => 7,
        K::WouldBlock => 8,
        K::InvalidData => 9,
        K::UnexpectedEof => 10,
        K::WriteZero => 11,
        K::Interrupted => 12,
        K::PermissionDenied => 13,
        K::AddrNotAvailable => 14,
        K::NotFound => 15,
        K::InvalidInput => 16,
        K::OutOfMemory => 17,
        _ => 1,
    }
}

/// Human name for an [`io_kind_code`] value.
#[inline]
pub const fn io_kind_name(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "other",
        2 => "connection-reset",
        3 => "connection-aborted",
        4 => "connection-refused",
        5 => "not-connected",
        6 => "broken-pipe",
        7 => "timed-out",
        8 => "would-block",
        9 => "invalid-data",
        10 => "unexpected-eof",
        11 => "write-zero",
        12 => "interrupted",
        13 => "permission-denied",
        14 => "addr-not-available",
        15 => "not-found",
        16 => "invalid-input",
        17 => "out-of-memory",
        _ => "unknown",
    }
}

/// One read-and-cleared session-error snapshot (see
/// [`IngressStatus::take_last_err`]). `venue_code` stores the venue's
/// signed code bit-cast to `u32` — cast back to `i32` for display
/// (Deribit codes are negative JSON-RPC codes).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionErrSnapshot {
    /// `ERR_SITE_*` code (0 = no error recorded).
    pub site: u8,
    /// [`io_kind_code`] class (0 = none).
    pub io_kind: u8,
    /// Venue numeric code bit-cast to u32 (0 = none).
    pub venue_code: u32,
}

/// Connection state of one ingress thread. Stored as a `u8`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum IngressState {
    /// Thread not running (not spawned, or exited).
    Down = 0,
    /// TCP/TLS connect + WS upgrade in progress.
    Connecting = 1,
    /// Upgraded and processing frames.
    Up = 2,
    /// Between reconnect attempts (venue outage / transport error).
    Backoff = 3,
}

impl IngressState {
    /// Decode a raw byte. Unknown values map to `Down` — a reader
    /// must never trust a torn/garbage byte into a bogus "Up".
    #[inline]
    pub const fn from_u8(b: u8) -> Self {
        match b {
            1 => Self::Connecting,
            2 => Self::Up,
            3 => Self::Backoff,
            _ => Self::Down,
        }
    }
}

/// Cache-aligned, single-writer status slot for one ingress thread.
///
/// Layout note: state + activity + 8 counters + the T1(a) diag
/// triple = 79 bytes → the slot spans two cache lines. That is
/// fine: there is exactly one writer and readers poll at human
/// cadence, so cross-line traffic is nil.
#[repr(C, align(64))]
pub struct IngressStatus {
    /// [`IngressState`] as raw byte.
    state: AtomicU8,
    /// Monotonic ns of the last byte received on this connection.
    last_activity_ns: AtomicU64,
    /// Messages (WS frames / JSON-RPC envelopes) parsed.
    msgs_total: AtomicU64,
    /// Payload bytes received.
    bytes_total: AtomicU64,
    /// Frames the parser rejected.
    parse_errors_total: AtomicU64,
    /// Venue sequence-chain breaks observed (§6.2 policy per venue).
    gaps_total: AtomicU64,
    /// Channel resubscribes triggered by integrity monitors.
    resubscribes_total: AtomicU64,
    /// Transport reconnects.
    reconnects_total: AtomicU64,
    /// Ring `try_push` failures — events dropped because the engine
    /// was not draining fast enough (D4).
    ring_drops_total: AtomicU64,
    /// Market-data rows (ticks/trades/books/summaries) parsed — the
    /// T1(b) backoff predicate: unlike `msgs_total`, control frames
    /// (acks, heartbeats, venue REJECTIONS) do not advance it, so a
    /// session that only received its own subscribe rejection cannot
    /// reset the reconnect schedule (outage §5.3).
    ticks_total: AtomicU64,
    /// WS2 (outage §5.2 remediation): subscribe args/channels dropped
    /// NON-FATALLY on a reconnect session (venue error event or
    /// missing-from-echo). Paired 1:1 with a `ChannelId::SubDrop`
    /// capture event by the emitting ingress.
    sub_drops_total: AtomicU64,
    /// WS10-A: venue-event lane pushes refused by a full ring.
    /// Funding loss ≠ tick loss (separate budget, separate alarm) —
    /// deliberately NOT folded into `ring_drops_total`.
    event_ring_drops_total: AtomicU64,
    /// WS10-B: depth-lane pushes refused by a full ring. Same
    /// separation rationale as `event_ring_drops_total`.
    depth_ring_drops_total: AtomicU64,
    /// VM2 V2: options-summary lane pushes refused by a full ring.
    /// Same separation rationale as the two above.
    opt_ring_drops_total: AtomicU64,
    /// T1(a) diag: `ERR_SITE_*` of the first fatal error this
    /// session (0 = none). First-error-wins; cleared by the venue
    /// loop via [`Self::take_last_err`] (same thread as the writer).
    last_err_site: AtomicU8,
    /// T1(a) diag: [`io_kind_code`] class of the first fatal error.
    last_err_io_kind: AtomicU8,
    /// T1(a) diag: venue numeric code bit-cast to u32 (0 = none).
    last_err_venue_code: AtomicU32,
}

impl IngressStatus {
    /// Fresh slot in `Down` state, all counters zero.
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(IngressState::Down as u8),
            last_activity_ns: AtomicU64::new(0),
            msgs_total: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            parse_errors_total: AtomicU64::new(0),
            gaps_total: AtomicU64::new(0),
            resubscribes_total: AtomicU64::new(0),
            reconnects_total: AtomicU64::new(0),
            ring_drops_total: AtomicU64::new(0),
            ticks_total: AtomicU64::new(0),
            sub_drops_total: AtomicU64::new(0),
            event_ring_drops_total: AtomicU64::new(0),
            depth_ring_drops_total: AtomicU64::new(0),
            opt_ring_drops_total: AtomicU64::new(0),
            last_err_site: AtomicU8::new(0),
            last_err_io_kind: AtomicU8::new(0),
            last_err_venue_code: AtomicU32::new(0),
        }
    }

    // ---- writer side (ingress thread only) ----

    /// Publish a state transition.
    #[inline(always)]
    pub fn set_state(&self, s: IngressState) {
        self.state.store(s as u8, Ordering::Relaxed);
    }

    /// Record activity (any byte received) at `now_ns`.
    #[inline(always)]
    pub fn touch_activity(&self, now_ns: u64) {
        self.last_activity_ns.store(now_ns, Ordering::Relaxed);
    }

    /// Count `n` parsed messages.
    #[inline(always)]
    pub fn add_msgs(&self, n: u64) {
        self.msgs_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Count `n` received payload bytes.
    #[inline(always)]
    pub fn add_bytes(&self, n: u64) {
        self.bytes_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Count one parser rejection.
    #[inline(always)]
    pub fn inc_parse_errors(&self) {
        self.parse_errors_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one sequence gap.
    #[inline(always)]
    pub fn inc_gaps(&self) {
        self.gaps_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one integrity-driven resubscribe.
    #[inline(always)]
    pub fn inc_resubscribes(&self) {
        self.resubscribes_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one transport reconnect.
    #[inline(always)]
    pub fn inc_reconnects(&self) {
        self.reconnects_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one dropped ring push (D4).
    #[inline(always)]
    pub fn inc_ring_drops(&self) {
        self.ring_drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count `n` parsed market-data rows (T1(b) — see `ticks_total`
    /// field docs for the control-frame exclusion rationale).
    #[inline(always)]
    pub fn add_ticks(&self, n: u64) {
        self.ticks_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Count one non-fatal subscribe drop (WS2 — see
    /// `sub_drops_total` field docs).
    #[inline(always)]
    pub fn inc_sub_drops(&self) {
        self.sub_drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one venue-event lane push refused by a full ring
    /// (WS10-A — see `event_ring_drops_total` field docs).
    #[inline(always)]
    pub fn inc_event_ring_drops(&self) {
        self.event_ring_drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one depth-lane push refused by a full ring (WS10-B).
    #[inline(always)]
    pub fn inc_depth_ring_drops(&self) {
        self.depth_ring_drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one options-summary lane push refused by a full ring
    /// (VM2 V2).
    #[inline(always)]
    pub fn inc_opt_ring_drops(&self) {
        self.opt_ring_drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// T1(a): record the venue's numeric error code for the current
    /// fatal error (bit-cast signed codes to u32; first-error-wins —
    /// a later error in the same session never overwrites).
    #[inline]
    pub fn note_venue_err_code(&self, code: u32) {
        if self.last_err_venue_code.load(Ordering::Relaxed) == 0 {
            self.last_err_venue_code.store(code, Ordering::Relaxed);
        }
    }

    /// T1(a): record where the session died + the io-kind class.
    /// First-error-wins: an inner site (venue-error / subscribe-
    /// missing) recorded during the drive step is NOT overwritten by
    /// the outer drive-site conversion that follows it.
    #[inline]
    pub fn note_session_err(&self, site: u8, io_kind: u8) {
        if self.last_err_site.load(Ordering::Relaxed) == 0 {
            self.last_err_site.store(site, Ordering::Relaxed);
            self.last_err_io_kind.store(io_kind, Ordering::Relaxed);
        }
    }

    // ---- reader side (metrics loop / TUI) ----

    /// Current state.
    #[inline]
    pub fn state(&self) -> IngressState {
        IngressState::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// Monotonic ns of last received byte (0 = never).
    #[inline]
    pub fn last_activity_ns(&self) -> u64 {
        self.last_activity_ns.load(Ordering::Relaxed)
    }

    /// Total parsed messages.
    #[inline]
    pub fn msgs_total(&self) -> u64 {
        self.msgs_total.load(Ordering::Relaxed)
    }

    /// Total payload bytes.
    #[inline]
    pub fn bytes_total(&self) -> u64 {
        self.bytes_total.load(Ordering::Relaxed)
    }

    /// Total parser rejections.
    #[inline]
    pub fn parse_errors_total(&self) -> u64 {
        self.parse_errors_total.load(Ordering::Relaxed)
    }

    /// Total sequence gaps.
    #[inline]
    pub fn gaps_total(&self) -> u64 {
        self.gaps_total.load(Ordering::Relaxed)
    }

    /// Total resubscribes.
    #[inline]
    pub fn resubscribes_total(&self) -> u64 {
        self.resubscribes_total.load(Ordering::Relaxed)
    }

    /// Total reconnects.
    #[inline]
    pub fn reconnects_total(&self) -> u64 {
        self.reconnects_total.load(Ordering::Relaxed)
    }

    /// Total ring drops (D4).
    #[inline]
    pub fn ring_drops_total(&self) -> u64 {
        self.ring_drops_total.load(Ordering::Relaxed)
    }

    /// Total parsed market-data rows (T1(b)).
    #[inline]
    pub fn ticks_total(&self) -> u64 {
        self.ticks_total.load(Ordering::Relaxed)
    }

    /// Total non-fatal subscribe drops (WS2).
    #[inline]
    pub fn sub_drops_total(&self) -> u64 {
        self.sub_drops_total.load(Ordering::Relaxed)
    }

    /// Total venue-event lane pushes refused by a full ring (WS10-A).
    #[inline]
    pub fn event_ring_drops_total(&self) -> u64 {
        self.event_ring_drops_total.load(Ordering::Relaxed)
    }

    /// Total depth-lane pushes refused by a full ring (WS10-B).
    #[inline]
    pub fn depth_ring_drops_total(&self) -> u64 {
        self.depth_ring_drops_total.load(Ordering::Relaxed)
    }

    /// Total options-summary lane pushes refused by a full ring
    /// (VM2 V2).
    #[inline(always)]
    pub fn opt_ring_drops_total(&self) -> u64 {
        self.opt_ring_drops_total.load(Ordering::Relaxed)
    }

    /// T1(a): read AND clear the session-error triple. Called by the
    /// cli venue loop right after `run()` returns — the SAME thread
    /// that wrote it (the venue loop and the run loop share one
    /// thread), so no cross-thread torn-read concern exists; the
    /// atomics are only for the metrics-reader side seeing a
    /// consistent-enough monitoring value.
    #[inline]
    pub fn take_last_err(&self) -> SessionErrSnapshot {
        let snap = SessionErrSnapshot {
            site: self.last_err_site.load(Ordering::Relaxed),
            io_kind: self.last_err_io_kind.load(Ordering::Relaxed),
            venue_code: self.last_err_venue_code.load(Ordering::Relaxed),
        };
        self.last_err_site.store(0, Ordering::Relaxed);
        self.last_err_io_kind.store(0, Ordering::Relaxed);
        self.last_err_venue_code.store(0, Ordering::Relaxed);
        snap
    }
}

impl Default for IngressStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_slot_is_down_and_zeroed() {
        let s = IngressStatus::new();
        assert_eq!(s.state(), IngressState::Down);
        assert_eq!(s.last_activity_ns(), 0);
        assert_eq!(s.ring_drops_total(), 0);
        assert_eq!(s.msgs_total(), 0);
    }

    #[test]
    fn state_transitions_publish() {
        let s = IngressStatus::new();
        s.set_state(IngressState::Connecting);
        assert_eq!(s.state(), IngressState::Connecting);
        s.set_state(IngressState::Up);
        assert_eq!(s.state(), IngressState::Up);
        s.set_state(IngressState::Backoff);
        assert_eq!(s.state(), IngressState::Backoff);
    }

    #[test]
    fn unknown_state_byte_decodes_to_down() {
        // Failure mode: garbage byte must never read as Up.
        assert_eq!(IngressState::from_u8(200), IngressState::Down);
        assert_eq!(IngressState::from_u8(4), IngressState::Down);
    }

    #[test]
    fn counters_accumulate() {
        let s = IngressStatus::new();
        s.add_msgs(3);
        s.add_bytes(1024);
        s.inc_parse_errors();
        s.inc_gaps();
        s.inc_resubscribes();
        s.inc_reconnects();
        s.inc_ring_drops();
        s.inc_ring_drops();
        s.touch_activity(42);
        assert_eq!(s.msgs_total(), 3);
        assert_eq!(s.bytes_total(), 1024);
        assert_eq!(s.parse_errors_total(), 1);
        assert_eq!(s.gaps_total(), 1);
        assert_eq!(s.resubscribes_total(), 1);
        assert_eq!(s.reconnects_total(), 1);
        assert_eq!(s.ring_drops_total(), 2);
        assert_eq!(s.last_activity_ns(), 42);
    }

    #[test]
    fn slot_is_cache_aligned() {
        assert_eq!(::core::mem::align_of::<IngressStatus>(), 64);
        // 1 + 8 + 11×8 + (1+1+4) = 103 B of fields → still two cache
        // lines (the T1/WS2/WS10 additions must never grow the slot
        // past 128).
        assert_eq!(::core::mem::size_of::<IngressStatus>(), 128);
    }

    #[test]
    fn ticks_are_counted_separately_from_msgs() {
        // T1(b): control frames advance msgs, data advances ticks —
        // the backoff predicate reads ONLY ticks.
        let s = IngressStatus::new();
        s.add_msgs(5); // acks / heartbeats / rejections
        assert_eq!(s.ticks_total(), 0);
        s.add_ticks(3);
        assert_eq!(s.ticks_total(), 3);
        assert_eq!(s.msgs_total(), 5);
    }

    #[test]
    fn session_err_first_wins_and_take_clears() {
        let s = IngressStatus::new();
        // Inner site records first (venue error with code)…
        s.note_venue_err_code((-32602i32) as u32);
        s.note_session_err(
            ERR_SITE_VENUE_ERROR,
            io_kind_code(std::io::ErrorKind::InvalidData),
        );
        // …the outer drive-site conversion must NOT overwrite it.
        s.note_session_err(ERR_SITE_DRIVE, io_kind_code(std::io::ErrorKind::Other));
        s.note_venue_err_code(1);
        let snap = s.take_last_err();
        assert_eq!(snap.site, ERR_SITE_VENUE_ERROR);
        assert_eq!(snap.io_kind, io_kind_code(std::io::ErrorKind::InvalidData));
        assert_eq!(snap.venue_code as i32, -32602);
        // take() cleared — the next session starts clean.
        assert_eq!(s.take_last_err(), SessionErrSnapshot::default());
        // And a fresh error records again after the clear.
        s.note_session_err(
            ERR_SITE_PUMP,
            io_kind_code(std::io::ErrorKind::ConnectionReset),
        );
        assert_eq!(s.take_last_err().site, ERR_SITE_PUMP);
    }

    #[test]
    fn event_ring_drops_counter_accumulates_independently() {
        // WS10-A: a refused lane push advances event_ring_drops ONLY —
        // never ring_drops (tick loss) or sub_drops.
        let s = IngressStatus::new();
        s.inc_event_ring_drops();
        s.inc_event_ring_drops();
        assert_eq!(s.event_ring_drops_total(), 2);
        assert_eq!(s.ring_drops_total(), 0);
        assert_eq!(s.sub_drops_total(), 0);
        assert_eq!(s.depth_ring_drops_total(), 0);
    }

    #[test]
    fn depth_ring_drops_counter_accumulates_independently() {
        // WS10-B: same separation law for the depth lane.
        let s = IngressStatus::new();
        s.inc_depth_ring_drops();
        assert_eq!(s.depth_ring_drops_total(), 1);
        assert_eq!(s.event_ring_drops_total(), 0);
        assert_eq!(s.ring_drops_total(), 0);
    }

    #[test]
    fn sub_drops_counter_accumulates_independently() {
        // WS2: a non-fatal drop advances sub_drops ONLY — never msgs,
        // ticks, or parse_errors (the drop is not data and not a
        // parser failure).
        let s = IngressStatus::new();
        s.inc_sub_drops();
        s.inc_sub_drops();
        assert_eq!(s.sub_drops_total(), 2);
        assert_eq!(s.msgs_total(), 0);
        assert_eq!(s.ticks_total(), 0);
        assert_eq!(s.parse_errors_total(), 0);
    }

    #[test]
    fn err_site_and_io_kind_names_resolve() {
        assert_eq!(err_site_name(0), "none");
        assert_eq!(err_site_name(ERR_SITE_DRIVE), "drive");
        assert_eq!(
            err_site_name(ERR_SITE_SUBSCRIBE_MISSING),
            "subscribe-missing"
        );
        assert_eq!(err_site_name(ERR_SITE_ESTABLISH), "establish-timeout");
        assert_eq!(err_site_name(200), "unknown");
        assert_eq!(io_kind_name(0), "none");
        assert_eq!(
            io_kind_name(io_kind_code(std::io::ErrorKind::TimedOut)),
            "timed-out"
        );
        assert_eq!(
            io_kind_name(io_kind_code(std::io::ErrorKind::BrokenPipe)),
            "broken-pipe"
        );
        assert_eq!(io_kind_name(200), "unknown");
    }

    #[test]
    fn cross_thread_visibility_smoke() {
        let s = std::sync::Arc::new(IngressStatus::new());
        let w = s.clone();
        let t = std::thread::spawn(move || {
            w.set_state(IngressState::Up);
            for _ in 0..1000 {
                w.inc_ring_drops();
            }
        });
        t.join().unwrap();
        assert_eq!(s.state(), IngressState::Up);
        assert_eq!(s.ring_drops_total(), 1000);
    }
}
