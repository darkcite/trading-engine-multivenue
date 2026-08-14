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

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

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
/// Layout note: state + activity + 7 counters = 65+ bytes → the slot
/// spans two cache lines. That is fine: there is exactly one writer
/// and readers poll at human cadence, so cross-line traffic is nil.
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
        // 1 + 8×8 = 65 B of fields → rounds to two cache lines.
        assert_eq!(::core::mem::size_of::<IngressStatus>(), 128);
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
