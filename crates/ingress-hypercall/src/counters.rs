// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Hypercall-specific counters beside the shared per-venue
//! `IngressStatus` family (plan §3 HC5): the slow-consumer law, the
//! quote shapes the venue's provider layer produces, the index / clock
//! gauges and the REST poller's health. Written by the ingress and
//! poller threads with `Relaxed` stores, read by the metrics exporter —
//! the `IngressStatus` discipline, one cache-line group per writer.

use core::sync::atomic::{AtomicU64, Ordering};

/// Why the venue closed the socket (the 1008 slow-consumer law, plan
/// §1.3), read from the CLOSE frame's reason JSON.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HcCloseCause {
    /// `cause: message_limit` — ≥ 16 384 queued messages (also what a
    /// multi-frame subscribe trips at once: the D3 law).
    MessageLimit = 0,
    /// `cause: byte_limit` — ≥ 8 MiB queued.
    ByteLimit = 1,
    /// `cause: queue_age` — the oldest queued message ≥ 5 s.
    QueueAge = 2,
    /// `cause: write_timeout` — one write blocked ≥ 5 s.
    WriteTimeout = 3,
    /// `error: slow_consumer` with a cause this build does not name.
    SlowOther = 4,
    /// Any other close (a plain 1000/1001, the 60 s pong timeout).
    Other = 5,
}

/// Number of [`HcCloseCause`] values.
pub const HC_CLOSE_CAUSES: usize = 6;

impl HcCloseCause {
    /// Metric label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MessageLimit => "message_limit",
            Self::ByteLimit => "byte_limit",
            Self::QueueAge => "queue_age",
            Self::WriteTimeout => "write_timeout",
            Self::SlowOther => "slow_other",
            Self::Other => "other",
        }
    }

    /// Every cause, in discriminant order.
    pub const ALL: [Self; HC_CLOSE_CAUSES] = [
        Self::MessageLimit,
        Self::ByteLimit,
        Self::QueueAge,
        Self::WriteTimeout,
        Self::SlowOther,
        Self::Other,
    ];
}

/// Ingress-thread counters (one writer).
#[repr(C, align(64))]
#[derive(Default)]
pub struct HcWsCounters {
    /// Server CLOSE frames by cause ([`HcCloseCause`] order).
    pub closes: [AtomicU64; HC_CLOSE_CAUSES],
    /// Subscribe sets queued (1 per session: connect + every resubscribe).
    pub subscribes: AtomicU64,
    /// Quotes with exactly one side.
    pub one_sided_quotes: AtomicU64,
    /// Quotes with neither side (`num_providers: 0`) — no tick.
    pub empty_quotes: AtomicU64,
    /// Two-sided quotes whose best bid exceeds the best ask.
    pub crossed_quotes: AtomicU64,
    /// `ProviderQuote` events captured.
    pub provider_quotes: AtomicU64,
    /// `ClockSynced` answers received.
    pub clock_syncs: AtomicU64,
    /// `MarketUpdate` events by action: created, expired, deleted, other.
    pub listings: [AtomicU64; 4],
    /// Prints on instruments outside the universe (unfiltered `trades`).
    pub foreign_trades: AtomicU64,
    /// Venue `Error` notices.
    pub venue_errors: AtomicU64,
    /// Last quote's `published_at − timestamp` (ms, gauge).
    pub quote_publish_lag_ms: AtomicU64,
    /// Instruments with at least one two-sided tick this process (gauge).
    pub quoted_instruments: AtomicU64,
    /// Most providers seen on one quote (gauge; a process-lifetime
    /// high-water mark).
    pub providers_max: AtomicU64,
    /// The index's age on the VENUE clock (ms, gauge): the freshest
    /// quote `published_at` minus the newest index entry's source stamp
    /// — both venue stamps, so no host clock enters it.
    pub index_age_ms: AtomicU64,
    /// Round trip of the last answered ClockSync (ms, gauge; monotonic
    /// host clock — the wall clock is never read).
    pub clock_rtt_ms: AtomicU64,
}

/// REST-poller counters (one writer: the poller thread).
#[repr(C, align(64))]
#[derive(Default)]
pub struct HcRestCounters {
    /// `/options-summary` requests answered 200.
    pub polls_ok: AtomicU64,
    /// Requests that failed (transport, status ≠ 200, unparseable).
    pub polls_err: AtomicU64,
    /// `OptSummary` rows emitted.
    pub opt_rows: AtomicU64,
    /// Rows for instruments outside the universe (skipped).
    pub foreign_rows: AtomicU64,
    /// Snapshot requests served (the `snapshot_resubscribe` law).
    pub snapshots: AtomicU64,
    /// Rows the handoff ring had no room for.
    pub handoff_drops: AtomicU64,
    /// Last poll round's wall duration (ms, gauge).
    pub last_round_ms: AtomicU64,
}

/// Every Hypercall counter; shared by `Arc` between the ingress thread,
/// the poller thread and the metrics exporter.
#[derive(Default)]
pub struct HcCounters {
    /// The ingress thread's.
    pub ws: HcWsCounters,
    /// The REST poller's.
    pub rest: HcRestCounters,
    /// Snapshot requests (ingress → poller): bumped by the ingress
    /// thread on every reconnect after a slow-consumer close; the poller
    /// serves one immediate round per change.
    pub snapshot_req: AtomicU64,
}

impl HcCounters {
    /// New, all zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// `+1`, relaxed (a statistics counter, never a synchronization point).
#[inline(always)]
pub fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Store a gauge, relaxed.
#[inline(always)]
pub fn set(c: &AtomicU64, v: u64) {
    c.store(v, Ordering::Relaxed);
}

/// Load, relaxed.
#[inline(always)]
#[must_use]
pub fn get(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}
