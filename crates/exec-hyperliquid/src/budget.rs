// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The address request-budget governor (plan §6.3).
//!
//! Hyperliquid meters L1 actions **per address**, not per connection
//! or per key:
//!
//! ```text
//!   allowance = 10_000 + (1 request per USDC of lifetime volume)
//! ```
//!
//! and an address that spends its allowance drops to **one request
//! every ten seconds**. That is not a rate limit in the usual sense —
//! it is a cliff, it is per-address so a reconnect does not clear it,
//! and at one request per ten seconds this engine cannot place, cancel
//! or reconcile at anything like the rate the strategy needs. An
//! exhausted address is a bricked address until volume accrues.
//!
//! The governor's whole job is to stop before the cliff rather than
//! discover it.
//!
//! ## Two asymmetries, both deliberate
//!
//! **Cancels are exempt.** Hyperliquid grants cancels a separate,
//! larger allowance, and more to the point: a halted engine must
//! always be able to flatten. A governor that refused a cancel because
//! the budget was low would strand exactly the position it was
//! protecting.
//!
//! **Volume is counted from VENUE FILLS ONLY** — never from what the
//! engine believes it traded. The whole point of the number is to
//! predict the venue's own accounting, and the engine's beliefs are
//! precisely what reconciliation exists to doubt.
//!
//! ## Why it owns its own persistence
//!
//! ⛏ §0.1-8: `userFees.dailyUserVlm` is a **daily** figure, not
//! cumulative-since-inception, and the venue exposes no "lifetime
//! volume" query. So the allowance cannot be recomputed from the
//! venue at boot — it has to be remembered. The state file is two
//! integers and the address they belong to.
//!
//! **A cold boot assumes the worst.** With no state file, or one
//! written for a different address, the governor starts at
//! `spent = initial_buffer` — i.e. `remaining() == traded`, which is
//! zero until venue fills accumulate. That refuses submits until the
//! engine has watched itself trade. The alternative, assuming the full
//! 10,000, would hand a fresh boot the whole allowance on the strength
//! of a missing file.

use core::sync::atomic::{AtomicU64, Ordering};

/// Hyperliquid's starting allowance for a fresh address.
pub const INITIAL_BUFFER: u64 = 10_000;

/// USDC (1e6) of volume that earns one request.
const USDC_PER_REQUEST_1E6: i64 = 1_000_000;

/// Why a submit was refused.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BudgetErr {
    /// Headroom is at or below the configured floor.
    Floor,
}

/// The per-address request budget.
///
/// Single-writer by construction: it lives on the dispatcher worker
/// thread, which is the only thread that sends an L1 action or reads a
/// venue fill. No atomics on the mutating path — the gauge mirror
/// below is the only thing another thread reads, and it is written
/// once per cold block.
#[repr(C, align(64))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AddressBudget {
    /// Every L1 action this address has sent, across boots.
    spent_requests: u64,
    /// Venue-observed traded notional, USDC 1e6. **Fills only.**
    traded_usdc_1e6: i64,
    /// The venue's starting grant.
    initial_buffer: u64,
    /// Refuse a submit when headroom drops to this.
    floor: u64,
    /// The address these numbers describe. A state file for a
    /// different address is not this address's history.
    address: [u8; 20],
    _pad: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<AddressBudget>() == 64);

impl AddressBudget {
    /// A cold-boot budget: assume the allowance is already spent.
    #[must_use]
    pub fn cold(address: [u8; 20], floor: u64) -> Self {
        Self {
            spent_requests: INITIAL_BUFFER,
            traded_usdc_1e6: 0,
            initial_buffer: INITIAL_BUFFER,
            floor,
            address,
            _pad: [0; 12],
        }
    }

    /// A budget restored from known figures.
    #[must_use]
    pub fn restored(
        address: [u8; 20],
        floor: u64,
        spent_requests: u64,
        traded_usdc_1e6: i64,
    ) -> Self {
        Self {
            spent_requests,
            traded_usdc_1e6,
            initial_buffer: INITIAL_BUFFER,
            floor,
            address,
            _pad: [0; 12],
        }
    }

    /// Requests believed to remain before the venue's cliff.
    ///
    /// Signed, and allowed to go negative: a governor that saturated
    /// at zero would report the same number for "just spent" and "a
    /// thousand over", and the difference is how far behind the
    /// engine's model of the venue has fallen.
    #[inline(always)]
    #[must_use]
    pub fn remaining(&self) -> i64 {
        // `spent_requests as i64` would WRAP: any value above
        // i64::MAX casts negative and turns this subtraction into an
        // addition, so a maximally-spent address would report
        // near-full headroom — the exact permissive failure this type
        // exists to prevent. Clamp instead.
        let spent = i64::try_from(self.spent_requests).unwrap_or(i64::MAX);
        (self.initial_buffer as i64)
            .saturating_add(self.traded_usdc_1e6 / USDC_PER_REQUEST_1E6)
            .saturating_sub(spent)
    }

    /// May a SUBMIT be sent?
    ///
    /// Checked before signing, not after: a signed action that is then
    /// discarded has still burned a nonce.
    #[inline(always)]
    pub fn may_submit(&self) -> Result<(), BudgetErr> {
        let floor = i64::try_from(self.floor).unwrap_or(i64::MAX);
        if self.remaining() <= floor {
            return Err(BudgetErr::Floor);
        }
        Ok(())
    }

    /// Cancels are always permitted. See the module docs: a halted
    /// engine must be able to flatten.
    #[inline(always)]
    #[must_use]
    pub const fn may_cancel(&self) -> bool {
        true
    }

    /// One L1 action left the host. Call for EVERY action, cancels
    /// included — the venue counts them even where it grants them a
    /// separate allowance, and a governor that undercounted would
    /// drift optimistic, which is the wrong direction.
    #[inline(always)]
    pub fn on_action_sent(&mut self) {
        self.spent_requests = self.spent_requests.saturating_add(1);
    }

    /// A venue fill was observed. `notional_usdc_1e6` must come from
    /// the fill, never from the order.
    #[inline(always)]
    pub fn on_venue_fill(&mut self, notional_usdc_1e6: i64) {
        // abs: a sell is volume too. `saturating_abs` because
        // i64::MIN has no positive counterpart and a hostile figure
        // must not panic on the fill path.
        self.traded_usdc_1e6 = self
            .traded_usdc_1e6
            .saturating_add(notional_usdc_1e6.saturating_abs());
    }

    /// Actions sent, across boots.
    #[inline]
    #[must_use]
    pub const fn spent(&self) -> u64 {
        self.spent_requests
    }

    /// Venue-observed traded notional, USDC 1e6.
    #[inline]
    #[must_use]
    pub const fn traded_1e6(&self) -> i64 {
        self.traded_usdc_1e6
    }

    /// The refusal threshold.
    #[inline]
    #[must_use]
    pub const fn floor(&self) -> u64 {
        self.floor
    }

    /// The address these figures belong to.
    #[inline]
    #[must_use]
    pub const fn address(&self) -> &[u8; 20] {
        &self.address
    }

    /// Serialise to the state-file line. Written by the worker in its
    /// cold block, so a `String` here is not a hot-path allocation.
    #[must_use]
    pub fn to_line(self) -> String {
        format!(
            "{}\t{}\t{}\n",
            crate::config::hex20(&self.address),
            self.spent_requests,
            self.traded_usdc_1e6
        )
    }

    /// Restore from a state-file line, for `address`.
    ///
    /// Returns `None` — meaning "use [`AddressBudget::cold`]" — for a
    /// malformed line OR a line written for a different address.
    /// Inheriting another address's allowance is exactly the mistake
    /// that spends a budget that was never granted.
    #[must_use]
    pub fn from_line(line: &str, address: [u8; 20], floor: u64) -> Option<Self> {
        let mut f = line.trim().split('\t');
        let addr_s = f.next()?;
        let spent: u64 = f.next()?.parse().ok()?;
        // A figure this side of absurd is a corrupt file, not a
        // history. Refusing lands the caller on the cold budget.
        if spent > i64::MAX as u64 {
            return None;
        }
        let traded: i64 = f.next()?.parse().ok()?;
        if f.next().is_some() {
            return None;
        }
        if !addr_s.eq_ignore_ascii_case(&crate::config::hex20(&address)) {
            return None;
        }
        Some(Self::restored(address, floor, spent, traded))
    }
}

/// A cross-thread mirror of [`AddressBudget::remaining`] for the
/// `/metrics` gauge and the `/state` section.
///
/// The worker writes it in its cold block; the metrics scrape reads
/// it. Relaxed both ways: a gauge that is one cold block stale is
/// fine, and making it stronger would put an ordering constraint on
/// the worker's hot loop to serve a reader that does not need one.
#[derive(Debug, Default)]
pub struct BudgetGauge(AtomicU64);

impl BudgetGauge {
    /// A gauge reading zero.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Publish the current headroom. Negative headroom publishes as
    /// zero: a gauge is unsigned, and "below zero" and "at zero" call
    /// for the same alarm.
    #[inline]
    pub fn publish(&self, remaining: i64) {
        self.0.store(remaining.max(0) as u64, Ordering::Relaxed);
    }

    /// Read the last published headroom.
    #[inline]
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Default location of the budget's own state file.
pub const DEFAULT_STATE_PATH: &str = "exec-budget.state";

/// Read the budget for `address` from `path`.
///
/// **Every failure returns a COLD budget**, which is the conservative
/// one: a missing file, an unreadable file, a malformed line and a
/// line written for a different address all mean "we do not know what
/// this address has spent", and the only safe answer to that is to
/// assume it has spent its grant. Returning a permissive default on a
/// read error is how a governor comes to authorise the very spending
/// it exists to prevent.
#[must_use]
pub fn load(path: &std::path::Path, address: [u8; 20], floor: u64) -> AddressBudget {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| AddressBudget::from_line(&t, address, floor))
        .unwrap_or_else(|| AddressBudget::cold(address, floor))
}

/// Write the budget durably (temp file, `sync_all`, rename).
///
/// Called from the worker's 5 s cold block, never the tick path.
///
/// # Errors
/// The temp write, the fsync or the rename failed; the message names
/// the path.
pub fn store(path: &std::path::Path, b: &AddressBudget) -> Result<(), String> {
    core_io::write_atomic(path, &b.to_line())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: [u8; 20] = [0xAB; 20];
    const OTHER: [u8; 20] = [0xCD; 20];

    /// The formula from the plan, and the direction each term moves.
    #[test]
    fn the_allowance_is_the_grant_plus_volume_minus_spend() {
        let mut b = AddressBudget::restored(ADDR, 100, 0, 0);
        assert_eq!(b.remaining(), 10_000);

        b.on_action_sent();
        assert_eq!(b.remaining(), 9_999, "an action costs one");

        // $1 of venue volume earns exactly one request.
        b.on_venue_fill(1_000_000);
        assert_eq!(b.remaining(), 10_000);

        // Sub-dollar fills earn nothing until they add up — integer
        // division, deliberately, because rounding UP would invent
        // allowance the venue did not grant.
        let mut b = AddressBudget::restored(ADDR, 0, 0, 0);
        for _ in 0..9 {
            b.on_venue_fill(100_000); // $0.10
        }
        assert_eq!(b.remaining(), 10_000, "$0.90 earns nothing");
        b.on_venue_fill(100_000);
        assert_eq!(b.remaining(), 10_001, "$1.00 earns one");
    }

    /// A sell is volume. Counting only buys would drift optimistic.
    #[test]
    fn both_sides_count_as_volume() {
        let mut b = AddressBudget::restored(ADDR, 0, 0, 0);
        b.on_venue_fill(-5_000_000);
        assert_eq!(b.traded_1e6(), 5_000_000);
        assert_eq!(b.remaining(), 10_005);
    }

    /// **A cold boot must not hand itself the allowance.**
    #[test]
    fn a_cold_boot_assumes_the_worst() {
        let b = AddressBudget::cold(ADDR, 100);
        assert_eq!(b.remaining(), 0, "no state file means no headroom");
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));
        // …and it recovers only as venue fills accumulate.
        let mut b = b;
        b.on_venue_fill(500_000_000); // $500
        assert_eq!(b.remaining(), 500);
        assert!(b.may_submit().is_ok());
    }

    /// The floor refuses submits and NEVER refuses a cancel.
    #[test]
    fn the_floor_never_strands_a_position() {
        let b = AddressBudget::restored(ADDR, 2_000, 8_500, 0);
        assert_eq!(b.remaining(), 1_500);
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));
        assert!(b.may_cancel(), "a halted engine must be able to flatten");

        // Exactly at the floor is refused: the floor is headroom to
        // KEEP, not headroom to spend down to.
        let b = AddressBudget::restored(ADDR, 2_000, 8_000, 0);
        assert_eq!(b.remaining(), 2_000);
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));

        let b = AddressBudget::restored(ADDR, 2_000, 7_999, 0);
        assert!(b.may_submit().is_ok());
    }

    /// Overspend reports how far past the cliff we are, rather than
    /// saturating and losing the distance.
    #[test]
    fn overspend_is_visible_rather_than_clamped() {
        let b = AddressBudget::restored(ADDR, 0, 11_000, 0);
        assert_eq!(b.remaining(), -1_000);
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));
        // The gauge, which is unsigned, shows zero.
        let g = BudgetGauge::new();
        g.publish(b.remaining());
        assert_eq!(g.get(), 0);
    }

    /// **A state file for a different address is not this address's
    /// history.** Inheriting it would spend an allowance never granted.
    #[test]
    fn a_state_file_for_another_address_is_refused() {
        let b = AddressBudget::restored(OTHER, 100, 500, 7_000_000);
        let line = b.to_line();
        assert!(AddressBudget::from_line(&line, OTHER, 100).is_some());
        assert!(
            AddressBudget::from_line(&line, ADDR, 100).is_none(),
            "another address's budget was inherited"
        );
    }

    #[test]
    fn the_state_line_round_trips_and_refuses_junk() {
        let b = AddressBudget::restored(ADDR, 250, 1_234, -9_876);
        let line = b.to_line();
        let back = AddressBudget::from_line(&line, ADDR, 250).expect("round trip");
        assert_eq!(back, b);
        // Trailing newline, whitespace, and case are tolerated.
        assert!(AddressBudget::from_line(&format!("  {}  ", line.trim()), ADDR, 250).is_some());
        assert!(AddressBudget::from_line(&line.to_uppercase(), ADDR, 250).is_some());
        // Junk is refused rather than half-parsed into a budget.
        for bad in [
            "",
            "\t\t",
            "0xabab\t1\t2",              // wrong-length address
            &format!("{}\tx\t2", crate::config::hex20(&ADDR)),
            &format!("{}\t1", crate::config::hex20(&ADDR)),
            &format!("{}\t1\t2\t3", crate::config::hex20(&ADDR)),
        ] {
            assert!(
                AddressBudget::from_line(bad, ADDR, 250).is_none(),
                "accepted junk: {bad:?}"
            );
        }
    }

    /// Cache-line sized and Copy — it sits on the worker's stack and
    /// is mirrored, never shared mutably.
    #[test]
    fn the_layout_is_one_cache_line() {
        assert_eq!(core::mem::size_of::<AddressBudget>(), 64);
        assert_eq!(core::mem::align_of::<AddressBudget>(), 64);
    }

    /// Every read failure must land on the CONSERVATIVE budget, not a
    /// permissive one. This is the property the governor rests on.
    #[test]
    fn every_unreadable_state_lands_on_the_cold_budget() {
        let dir = std::env::temp_dir().join(format!("mv-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("exec-budget.state");

        // Absent.
        assert_eq!(load(&p, ADDR, 100), AddressBudget::cold(ADDR, 100));
        // Empty, truncated, junk, and a line for ANOTHER address.
        for bad in [
            String::new(),
            "\n".to_owned(),
            "garbage".to_owned(),
            AddressBudget::restored(OTHER, 100, 5, 5).to_line(),
        ] {
            std::fs::write(&p, &bad).unwrap();
            assert_eq!(
                load(&p, ADDR, 100),
                AddressBudget::cold(ADDR, 100),
                "permissive budget from {bad:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A good file round-trips through the durable write, and the temp
    /// file is not left behind under a name that belongs to no file.
    #[test]
    fn a_stored_budget_comes_back_intact() {
        let dir = std::env::temp_dir().join(format!("mv-budget-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("exec-budget.state");

        let b = AddressBudget::restored(ADDR, 250, 4_242, 77_000_000);
        store(&p, &b).expect("store");
        assert_eq!(load(&p, ADDR, 250), b);

        let mut t = p.clone().into_os_string();
        t.push(".tmp");
        assert!(!std::path::PathBuf::from(t).exists(), "temp left behind");
        // The old, wrong temp name must not be created either.
        assert!(!p.with_extension("tsv.tmp").exists());

        // A rewrite REPLACES rather than appends.
        let b2 = AddressBudget::restored(ADDR, 250, 4_243, 78_000_000);
        store(&p, &b2).expect("store");
        assert_eq!(load(&p, ADDR, 250), b2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hostile figures from a venue row must not panic the fill path.
    #[test]
    fn extreme_inputs_saturate_rather_than_panic() {
        let mut b = AddressBudget::restored(ADDR, 0, 0, i64::MAX);
        b.on_venue_fill(i64::MAX);
        assert_eq!(b.traded_1e6(), i64::MAX);
        let _ = b.remaining();

        // THE WRAP. `spent as i64` would make this report +10_001 —
        // a maximally-spent address claiming near-full headroom.
        let b = AddressBudget::restored(ADDR, 0, u64::MAX, 0);
        assert!(
            b.remaining() <= 0,
            "a maximally-spent address reported {} headroom",
            b.remaining()
        );
        assert_eq!(b.may_submit(), Err(BudgetErr::Floor));
        let mut b = b;
        b.on_action_sent();
        assert_eq!(b.spent(), u64::MAX);
        assert!(b.remaining() <= 0);

        // …and such a figure in a state FILE is refused outright, so
        // the caller lands cold rather than restoring nonsense.
        let line = format!("{}\t{}\t0", crate::config::hex20(&ADDR), u64::MAX);
        assert!(AddressBudget::from_line(&line, ADDR, 0).is_none());

        let mut b = AddressBudget::restored(ADDR, 0, 0, 0);
        b.on_venue_fill(i64::MIN);
        let _ = b.remaining();
    }
}
