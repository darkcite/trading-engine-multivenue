// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The arm's and pm-gw's tallies for `/metrics` (PM15-8c): one block of
//! relaxed atomics per writer — the arm (engine thread) and pm-gw (with
//! its redeemer) — each on its own cache lines, published at most once a
//! [`PUBLISH_EVERY_NS`], read by the metrics mirror as `engine_pm15_arm_*`
//! and `engine_pm15_gw_*`. A paper boot never builds the arm: the block
//! stays zero. Statistics only — never a synchronization point.

use core::sync::atomic::{AtomicI64, Ordering};

/// The publish cadence of either writer, ns.
pub const PUBLISH_EVERY_NS: u64 = 1_000_000_000;

/// The arm's words: its tallies, then [`ARM_LEVELS`] levels.
pub const ARM_WORDS: usize = 30;
/// The arm's levels (the last words): halted, reconciled, the account's
/// worth, the session anchor, the drift.
pub const ARM_LEVELS: usize = 5;
/// The arm's names, in word order.
pub const ARM_NAMES: [&str; ARM_WORDS] = [
    "submitted",
    "refused_shape",
    "refused_family",
    "refused_halted",
    "refused_full",
    "acks",
    "refused",
    "no_match",
    "in_doubt",
    "recons",
    "gaps",
    "fills",
    "repeats",
    "confirmed",
    "reversed",
    "unknown",
    "done",
    "out_full",
    "evicted_young",
    "reads_agreed",
    "reads_disagreed",
    "legs_redeemed",
    "legs_lost",
    "redeem_asks",
    "ledger_full",
    "halted",
    "reconciled",
    "worth_usd_1e6",
    "anchor_usd_1e6",
    "drift_usd_1e6",
];

/// pm-gw's words: its tallies and its redeemer's, then [`GW_LEVELS`]
/// level.
pub const GW_WORDS: usize = 28;
/// pm-gw's levels (the last word): halted.
pub const GW_LEVELS: usize = 1;
/// pm-gw's names, in word order.
pub const GW_NAMES: [&str; GW_WORDS] = [
    "specs",
    "specs_refused",
    "posts",
    "resends",
    "acks",
    "refused",
    "refused_local",
    "in_doubt",
    "recon_requests",
    "recon_failures",
    "recon_fills",
    "recon_done",
    "account_requests",
    "account_failures",
    "account_reads",
    "redeem_off",
    "redeem_no_row",
    "redeem_queued",
    "redeem_dupes",
    "redeem_queue_full",
    "redeem_batches",
    "redeem_batches_done",
    "redeemed",
    "redeem_batches_failed",
    "redeem_dropped",
    "redeem_request_failures",
    "redeem_polls",
    "halted",
];

#[repr(C, align(64))]
struct Block<const N: usize>([AtomicI64; N]);

impl<const N: usize> Block<N> {
    const fn new() -> Self {
        Self([const { AtomicI64::new(0) }; N])
    }

    #[inline]
    fn store(&self, v: &[i64; N]) {
        let mut i = 0usize;
        while i < N {
            self.0[i].store(v[i], Ordering::Relaxed);
            i += 1;
        }
    }

    #[inline]
    fn load(&self, i: usize) -> i64 {
        match self.0.get(i) {
            Some(w) => w.load(Ordering::Relaxed),
            None => 0,
        }
    }
}

/// The two blocks (module docs). Boot-built behind an `Arc`.
pub struct PmExecStats {
    arm: Block<ARM_WORDS>,
    gw: Block<GW_WORDS>,
}

impl Default for PmExecStats {
    fn default() -> Self {
        Self::new()
    }
}

impl PmExecStats {
    /// All zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            arm: Block::new(),
            gw: Block::new(),
        }
    }

    /// The arm's word `i` (0 past the table).
    #[inline]
    #[must_use]
    pub fn arm(&self, i: usize) -> i64 {
        self.arm.load(i)
    }

    /// pm-gw's word `i` (0 past the table).
    #[inline]
    #[must_use]
    pub fn gw(&self, i: usize) -> i64 {
        self.gw.load(i)
    }

    #[inline]
    pub(crate) fn set_arm(&self, v: &[i64; ARM_WORDS]) {
        self.arm.store(v);
    }

    #[inline]
    pub(crate) fn set_gw(&self, v: &[i64; GW_WORDS]) {
        self.gw.store(v);
    }
}

/// A tally as a word (a u64 past `i64::MAX` saturates).
#[inline]
pub(crate) fn word(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

const _: () = assert!(core::mem::align_of::<PmExecStats>() == 64);
const _: () = assert!(ARM_LEVELS < ARM_WORDS && GW_LEVELS < GW_WORDS);
