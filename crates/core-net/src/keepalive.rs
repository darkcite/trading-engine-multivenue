// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Keepalive — proactive pings + idle-timeout detection (D5/D6)
//!
//! Every venue cuts idle connections (OKX at 30 s, Hyperliquid at
//! 60 s; Deribit closes on unanswered `test_request`). Before Phase
//! 8a nothing read `Driver.last_activity_ns` (D5) and no feed ever
//! pinged proactively (D6) — half-open TCP sessions were only caught
//! on `Ok(0)`.
//!
//! This module is the *scheduler* only. The venue-specific ping
//! bytes stay in each ingress crate (OKX: literal `ping` text frame;
//! Hyperliquid: `{"method":"ping"}`; Deribit: JSON-RPC `public/test`
//! reply driven by its heartbeat machine; Binance/Polymarket: WS
//! protocol-level ping). The run loop calls [`Keepalive::poll`] once
//! per iteration in `Steady` and acts on the returned
//! [`KeepaliveAction`].

/// Static per-venue keepalive configuration.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KeepaliveCfg {
    /// Send a ping when neither inbound bytes nor our own ping have
    /// happened for this long. Pick `< venue idle cutoff` with margin
    /// (e.g. OKX cutoff 30 s → interval 25 s).
    pub ping_interval_ns: u64,
    /// Force a reconnect when *no inbound bytes at all* arrive for
    /// this long — the ping went unanswered or the TCP session is
    /// half-open. Must exceed `ping_interval_ns`.
    pub idle_timeout_ns: u64,
}

/// What the run loop must do right now.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeepaliveAction {
    /// Nothing due.
    None,
    /// Queue the venue-specific ping frame and call
    /// [`Keepalive::mark_ping_sent`].
    SendPing,
    /// The connection is dead by policy — tear down and reconnect.
    Reconnect,
}

/// Per-connection keepalive state. Single-owner (the ingress thread).
#[derive(Copy, Clone, Debug)]
pub struct Keepalive {
    cfg: KeepaliveCfg,
    /// Monotonic ns of the last ping we queued (0 = none yet).
    last_ping_ns: u64,
}

impl Keepalive {
    /// New state for one connection.
    pub const fn new(cfg: KeepaliveCfg) -> Self {
        debug_assert!(cfg.idle_timeout_ns > cfg.ping_interval_ns);
        Self {
            cfg,
            last_ping_ns: 0,
        }
    }

    /// Reset on reconnect.
    #[inline]
    pub fn reset(&mut self) {
        self.last_ping_ns = 0;
    }

    /// Record that the ping frame was queued at `now_ns`.
    #[inline]
    pub fn mark_ping_sent(&mut self, now_ns: u64) {
        self.last_ping_ns = now_ns;
    }

    /// Decide the action at `now_ns` given the connection's
    /// `last_activity_ns` (monotonic ns of the last inbound byte;
    /// `0` = nothing received yet this session, in which case the
    /// caller should pass the session-start time instead — a
    /// connection that never delivers a byte must still time out).
    ///
    /// Order: reconnect dominates ping. Zero-alloc, branch-light.
    #[inline]
    pub fn poll(&mut self, now_ns: u64, last_activity_ns: u64) -> KeepaliveAction {
        let idle_for = now_ns.saturating_sub(last_activity_ns);
        if idle_for >= self.cfg.idle_timeout_ns {
            return KeepaliveAction::Reconnect;
        }
        // Quiet time = time since we last heard OR pinged, whichever
        // is more recent — prevents a ping storm while the venue is
        // legitimately silent between our probes.
        let anchor = if self.last_ping_ns > last_activity_ns {
            self.last_ping_ns
        } else {
            last_activity_ns
        };
        if now_ns.saturating_sub(anchor) >= self.cfg.ping_interval_ns {
            return KeepaliveAction::SendPing;
        }
        KeepaliveAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: KeepaliveCfg = KeepaliveCfg {
        ping_interval_ns: 25_000_000_000, // 25 s
        idle_timeout_ns: 40_000_000_000,  // 40 s
    };

    #[test]
    fn quiet_connection_gets_ping_then_reconnect() {
        let mut k = Keepalive::new(CFG);
        let t0 = 1_000_000_000u64;
        // Fresh activity → nothing.
        assert_eq!(k.poll(t0 + 1_000_000_000, t0), KeepaliveAction::None);
        // 25 s quiet → ping.
        assert_eq!(k.poll(t0 + 25_000_000_000, t0), KeepaliveAction::SendPing);
        k.mark_ping_sent(t0 + 25_000_000_000);
        // Ping sent, still within idle budget → no second ping yet.
        assert_eq!(
            k.poll(t0 + 26_000_000_000, t0),
            KeepaliveAction::None,
            "must not spam pings while awaiting pong"
        );
        // 40 s with zero inbound → dead by policy.
        assert_eq!(k.poll(t0 + 40_000_000_000, t0), KeepaliveAction::Reconnect);
    }

    #[test]
    fn pong_resets_the_clock() {
        let mut k = Keepalive::new(CFG);
        let t0 = 1_000_000_000u64;
        assert_eq!(k.poll(t0 + 25_000_000_000, t0), KeepaliveAction::SendPing);
        k.mark_ping_sent(t0 + 25_000_000_000);
        // Venue answers at +26 s → activity moves forward; no action
        // until 26+25 = 51 s.
        let pong = t0 + 26_000_000_000;
        assert_eq!(k.poll(t0 + 30_000_000_000, pong), KeepaliveAction::None);
        assert_eq!(
            k.poll(pong + 25_000_000_000, pong),
            KeepaliveAction::SendPing
        );
    }

    #[test]
    fn busy_connection_never_pings() {
        let mut k = Keepalive::new(CFG);
        let mut now = 1_000_000_000u64;
        for _ in 0..100 {
            now += 1_000_000_000;
            // Traffic every second → activity always equals now.
            assert_eq!(k.poll(now, now), KeepaliveAction::None);
        }
    }

    #[test]
    fn reconnect_dominates_ping_when_both_due() {
        let mut k = Keepalive::new(CFG);
        let t0 = 1_000_000_000u64;
        // 41 s quiet, never pinged: both conditions true → Reconnect.
        assert_eq!(k.poll(t0 + 41_000_000_000, t0), KeepaliveAction::Reconnect);
    }
}
