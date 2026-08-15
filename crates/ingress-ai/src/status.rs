//! Single-writer status slot for the AI ingress thread.
//!
//! Same machinery as `core_metrics::IngressStatus` (the D7 pattern):
//! one cache-aligned atomic slot allocated at boot and shared via
//! `Arc`; the cli metrics loop mirrors it into registry
//! counters/gauges each report period (item 6). All accesses are
//! `Relaxed` — monitoring state, no synchronization derived from it.
//!
//! Writer discipline (per-field single writer):
//! * Every field except `expired_total` is written by the **ingress
//!   thread only**.
//! * `expired_total` is written by the **engine drain site only**
//!   (TTL-expiry is observable at pop, not at accept — item 6 wires
//!   it). It lives here so the whole `engine_ingress_ai_*` family
//!   mirrors from one slot.

use core::sync::atomic::{AtomicU64, Ordering};

/// Cache-aligned status slot for the AI ingress. Field semantics map
/// 1:1 onto the design §4.4 metric family
/// (`engine_ingress_ai_*_total` + the heartbeat gauge).
#[repr(C, align(64))]
pub struct AiIngressStatus {
    /// Frames that passed len + HMAC + shape + seq and entered
    /// capture-and-push (§4.4 step 6) — includes heartbeats.
    cmds_total: AtomicU64,
    /// HMAC tag mismatches (connection-fatal).
    hmac_fail_total: AtomicU64,
    /// Length-field violations + torn-frame residue at connection
    /// close (connection-fatal).
    protocol_err_total: AtomicU64,
    /// Shape-table violations (frame discarded, connection kept).
    malformed_total: AtomicU64,
    /// Forward sequence-gap events (frame accepted).
    seq_gap_total: AtomicU64,
    /// Sequence regressions (frame discarded, connection kept).
    seq_regress_total: AtomicU64,
    /// Ring `try_push` failures — commands dropped because the engine
    /// was not draining fast enough.
    ring_drops_total: AtomicU64,
    /// Commands dropped TTL-expired at the engine drain site.
    /// **Writer: engine thread (item 6)** — see module docs.
    expired_total: AtomicU64,
    /// Connections refused: second client while one is held, or
    /// peer-credential euid mismatch.
    rejected_conns_total: AtomicU64,
    /// Engine-monotonic ns of the last Heartbeat accepted; 0 = never.
    /// The `engine_ingress_ai_last_heartbeat_age_ns` gauge is derived
    /// by readers as `now_ns - last_heartbeat_ns` (item 6 mirrors).
    last_heartbeat_ns: AtomicU64,
}

impl AiIngressStatus {
    /// Fresh slot, all counters zero.
    pub const fn new() -> Self {
        Self {
            cmds_total: AtomicU64::new(0),
            hmac_fail_total: AtomicU64::new(0),
            protocol_err_total: AtomicU64::new(0),
            malformed_total: AtomicU64::new(0),
            seq_gap_total: AtomicU64::new(0),
            seq_regress_total: AtomicU64::new(0),
            ring_drops_total: AtomicU64::new(0),
            expired_total: AtomicU64::new(0),
            rejected_conns_total: AtomicU64::new(0),
            last_heartbeat_ns: AtomicU64::new(0),
        }
    }

    // ---- writer side ----

    /// Count one accepted command.
    #[inline(always)]
    pub fn inc_cmds(&self) {
        self.cmds_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one HMAC failure.
    #[inline(always)]
    pub fn inc_hmac_fail(&self) {
        self.hmac_fail_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one protocol error.
    #[inline(always)]
    pub fn inc_protocol_err(&self) {
        self.protocol_err_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one malformed frame.
    #[inline(always)]
    pub fn inc_malformed(&self) {
        self.malformed_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one sequence-gap event.
    #[inline(always)]
    pub fn inc_seq_gap(&self) {
        self.seq_gap_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one sequence regression.
    #[inline(always)]
    pub fn inc_seq_regress(&self) {
        self.seq_regress_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one ring drop.
    #[inline(always)]
    pub fn inc_ring_drops(&self) {
        self.ring_drops_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one TTL expiry at pop. **Engine drain site only** (item 6).
    #[inline(always)]
    pub fn inc_expired(&self) {
        self.expired_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one rejected connection.
    #[inline(always)]
    pub fn inc_rejected_conns(&self) {
        self.rejected_conns_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Publish the accept time of a Heartbeat command.
    #[inline(always)]
    pub fn set_last_heartbeat_ns(&self, now_ns: u64) {
        self.last_heartbeat_ns.store(now_ns, Ordering::Relaxed);
    }

    // ---- reader side (cli mirror / TUI / tests) ----

    /// Accepted commands.
    #[inline]
    pub fn cmds(&self) -> u64 {
        self.cmds_total.load(Ordering::Relaxed)
    }

    /// HMAC failures.
    #[inline]
    pub fn hmac_fail(&self) -> u64 {
        self.hmac_fail_total.load(Ordering::Relaxed)
    }

    /// Protocol errors.
    #[inline]
    pub fn protocol_err(&self) -> u64 {
        self.protocol_err_total.load(Ordering::Relaxed)
    }

    /// Malformed frames.
    #[inline]
    pub fn malformed(&self) -> u64 {
        self.malformed_total.load(Ordering::Relaxed)
    }

    /// Sequence-gap events.
    #[inline]
    pub fn seq_gap(&self) -> u64 {
        self.seq_gap_total.load(Ordering::Relaxed)
    }

    /// Sequence regressions.
    #[inline]
    pub fn seq_regress(&self) -> u64 {
        self.seq_regress_total.load(Ordering::Relaxed)
    }

    /// Ring drops.
    #[inline]
    pub fn ring_drops(&self) -> u64 {
        self.ring_drops_total.load(Ordering::Relaxed)
    }

    /// TTL expiries at pop.
    #[inline]
    pub fn expired(&self) -> u64 {
        self.expired_total.load(Ordering::Relaxed)
    }

    /// Rejected connections.
    #[inline]
    pub fn rejected_conns(&self) -> u64 {
        self.rejected_conns_total.load(Ordering::Relaxed)
    }

    /// Engine-monotonic ns of the last accepted Heartbeat (0 = never).
    #[inline]
    pub fn last_heartbeat_ns(&self) -> u64 {
        self.last_heartbeat_ns.load(Ordering::Relaxed)
    }
}

impl Default for AiIngressStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_independently() {
        let s = AiIngressStatus::new();
        s.inc_cmds();
        s.inc_cmds();
        s.inc_hmac_fail();
        s.inc_protocol_err();
        s.inc_malformed();
        s.inc_seq_gap();
        s.inc_seq_regress();
        s.inc_ring_drops();
        s.inc_expired();
        s.inc_rejected_conns();
        s.set_last_heartbeat_ns(99);
        assert_eq!(s.cmds(), 2);
        assert_eq!(s.hmac_fail(), 1);
        assert_eq!(s.protocol_err(), 1);
        assert_eq!(s.malformed(), 1);
        assert_eq!(s.seq_gap(), 1);
        assert_eq!(s.seq_regress(), 1);
        assert_eq!(s.ring_drops(), 1);
        assert_eq!(s.expired(), 1);
        assert_eq!(s.rejected_conns(), 1);
        assert_eq!(s.last_heartbeat_ns(), 99);
    }

    #[test]
    fn slot_is_cache_aligned() {
        assert_eq!(::core::mem::align_of::<AiIngressStatus>(), 64);
    }
}
