// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # stats — the arm's and jup-gw's tallies for `/metrics` (PMJ J3)
//!
//! One block of relaxed atomics per writer — the arm (engine thread) and
//! jup-gw — each on its own cache lines, published at most once a
//! [`PUBLISH_EVERY_NS`], read by the metrics mirror as `engine_pmj_arm_*`
//! and `engine_pmj_gw_*` (the live arm's boot registers them, J5; J4's
//! paper boot registers only the member's `engine_pmj_*`). A paper boot never builds
//! the arm: the block stays zero. Statistics only — never a
//! synchronization point.
//!
//! The latency histograms are the duplex's own `GatewayStats` series (the
//! fixed metrics families), read as Jupiter's:
//!
//! | series | Jupiter's |
//! |---|---|
//! | `push_to_write` | the member's decision → the build's request |
//! | `post_rtt` | the build (`POST /orders`) round trip |
//! | `rest_rtt` | the land (`POST /execute`) round trip |
//! | `cancel_rtt` | a status poll's round trip |
//! | `fill_venue_to_local` | the execute → the fill read (the keeper's fill time) |

use core::sync::atomic::{AtomicI64, Ordering};

/// The publish cadence of either writer, ns.
pub const PUBLISH_EVERY_NS: u64 = 1_000_000_000;

/// The arm's words.
pub const ARM_WORDS: usize = 21;
/// The arm's names, in word order (the last five are levels).
pub const ARM_NAMES: [&str; ARM_WORDS] = [
    "submitted",
    "refused_shape",
    "refused_family",
    "refused_size",
    "refused_halted",
    "refused_full",
    "landed",
    "refused",
    "in_doubt",
    "fills",
    "done",
    "lost",
    "legs_lost",
    "claims_ready",
    "out_full",
    "over_limit",
    "halted",
    "reconciled",
    "worth_usd_1e6",
    "anchor_usd_1e6",
    "usdc_usd_1e6",
];

/// jup-gw's words.
pub const GW_WORDS: usize = 36;
/// jup-gw's names, in word order (the last five are levels: the halt
/// reason, the USDC and SOL gauges, -1 = not read; the debit halt's
/// reference, -1 = none yet, and the deposits sent since it).
pub const GW_NAMES: [&str; GW_WORDS] = [
    "specs",
    "specs_refused",
    "builds",
    "built",
    "guard_refused",
    "price_refused",
    "executes",
    "landed",
    "exec_failed",
    "in_doubt",
    "refused",
    "venue_refused",
    "region_refused",
    "polls",
    "poll_failures",
    "recons",
    "recon_misses",
    "fills",
    "done",
    "lost",
    "balance_reads",
    "balance_failures",
    "position_reads",
    "position_failures",
    "claims_held",
    "claims_refused",
    "held_writes",
    "held_full",
    "debit_halts",
    "released",
    "unknown_positions",
    "halt_reason",
    "usdc_micro",
    "sol_lamports",
    "debit_ref_micro",
    "debit_allow_micro",
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
pub struct JupExecStats {
    arm: Block<ARM_WORDS>,
    gw: Block<GW_WORDS>,
}

impl Default for JupExecStats {
    fn default() -> Self {
        Self::new()
    }
}

impl JupExecStats {
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

    /// jup-gw's word `i` (0 past the table).
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

const _: () = assert!(core::mem::align_of::<JupExecStats>() == 64);
