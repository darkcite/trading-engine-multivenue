// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The pool side: every HyperEVM pool-event signal into the AMM book (the
//! judge's own state machine), the member's tick maps beside it, and the
//! per-pool basis EMA.
//!
//! **The book first, the map second.** `AmmBook::observe` decides what the
//! chain says about price, liquidity, fee and staleness exactly as the
//! paper matcher does; the member then keeps what the book deliberately
//! does not — the initialised-tick map the sizing walk crosses:
//!
//! * `SNAPSHOT` opens staging (coverage, node count, the MAP's spacing —
//!   an Algebra map is a linked list, so it loads with spacing 1), each
//!   `TICK` appends one node, and the snapshot `STATE` loads the map. A
//!   count that does not match, a node the codec refuses, or a map
//!   `TickMap::load` refuses leaves the pool UNTRADED (and stale in the
//!   member's book) until its next snapshot.
//! * `LIQUIDITY` (Mint/Burn) is applied to the map; a refusal means the
//!   map and the chain disagree — same outcome.
//! * `GAP` (chain-wide or per pool) invalidates the maps it covers: a
//!   Mint/Burn may have been missed. The ingress re-snapshots on resync.

use core_amm::payload::{decode, PoolEvent, FAMILY_ALGEBRA};
use core_amm::TickNode;
use core_fill::AmmObs;
use core_types::{NsTs, Signal};

use crate::{HyparbStrategy, MAP_NODES, ONE_BPS_1E6};

/// A basis beyond ±50 % is a broken pool or a wrong coin mapping, not a
/// basis: the sample is clamped so it cannot poison the EMA.
const BASIS_CLAMP_BPS_1E6: i64 = ONE_BPS_1E6 / 2;
/// Price operands are scaled down to at most this before the basis
/// ratio, so `diff × ONE_BPS_1E6` fits an `i128`.
const BASIS_OPERAND_MAX: u128 = 100_000_000_000_000_000_000_000_000;

impl HyparbStrategy {
    /// Apply one HyperEVM pool-event signal. `Some(p)` when member pool
    /// `p` changed in a way worth re-evaluating (a price, a fee, a
    /// position, a completed snapshot).
    pub(crate) fn apply_pool_signal(&mut self, signal: &Signal) -> Option<usize> {
        self.counters.pool_events = self.counters.pool_events.wrapping_add(1);
        let obs = self.book.observe(signal.sym, &signal.payload);
        match obs {
            AmmObs::Refused => {
                self.counters.pool_refused = self.counters.pool_refused.wrapping_add(1);
                return None;
            }
            AmmObs::Head { .. } => return None,
            AmmObs::Gap => {
                let mut p = 0usize;
                while p < self.params.n_pools {
                    self.pools[p].map_ok = false;
                    p += 1;
                }
                self.abandon_staging();
                return None;
            }
            AmmObs::Pool { .. } | AmmObs::Quiet => {}
        }
        // Pools the member does not trade are tracked by the book alone.
        let p = self.pool_of(signal.sym)?;
        // The book accepted it, so the codec does.
        let ev = decode(&signal.payload)?;
        match ev {
            PoolEvent::Snapshot {
                lo,
                hi,
                nodes,
                spacing,
                family,
                ..
            } => {
                self.abandon_staging();
                self.maps[p].begin_stage();
                let st = &mut self.staging;
                st.pool = p;
                st.n = 0;
                st.expect = nodes as usize;
                st.lo = lo;
                st.hi = hi;
                st.spacing = if family == FAMILY_ALGEBRA { 1 } else { spacing };
                st.broken = st.expect > MAP_NODES;
                self.pools[p].map_ok = false;
                None
            }
            PoolEvent::Tick { tick, net, gross } => {
                let st = &mut self.staging;
                if st.pool != p || st.broken {
                    return None;
                }
                if st.n >= st.expect {
                    st.broken = true;
                    return None;
                }
                match (
                    TickNode::with_gross(tick, net, gross),
                    self.maps[p].stage_slot(st.n),
                ) {
                    (Some(node), Some(slot)) => {
                        *slot = node;
                        st.n += 1;
                    }
                    _ => st.broken = true,
                }
                None
            }
            PoolEvent::State { snapshot: true, .. } => {
                if self.staging.pool == p {
                    self.finish_staging(p);
                }
                if self.pools[p].map_ok {
                    Some(p)
                } else {
                    None
                }
            }
            PoolEvent::State {
                snapshot: false, ..
            }
            | PoolEvent::Fee { .. } => Some(p),
            PoolEvent::Liquidity {
                burn,
                tick_lower,
                tick_upper,
                amount,
                ..
            } => {
                // `Quiet`: the book holds the pool unsnapshotted — so is
                // the map, and nothing is applied to either.
                if !self.pools[p].map_ok || obs == AmmObs::Quiet {
                    return None;
                }
                let ok = if amount > i128::MAX as u128 {
                    false
                } else {
                    let delta = if burn {
                        -(amount as i128)
                    } else {
                        amount as i128
                    };
                    self.maps[p]
                        .apply_position(tick_lower, tick_upper, delta)
                        .is_ok()
                };
                if !ok {
                    self.refuse_map(p);
                }
                Some(p)
            }
            PoolEvent::Gap { .. } => {
                self.pools[p].map_ok = false;
                if self.staging.pool == p {
                    self.abandon_staging();
                }
                None
            }
            PoolEvent::Swap { .. } | PoolEvent::Head { .. } => None,
        }
    }

    /// Drop a snapshot in progress (counted as a refused map).
    fn abandon_staging(&mut self) {
        if self.staging.pool != usize::MAX {
            self.staging.pool = usize::MAX;
            self.counters.maps_refused = self.counters.maps_refused.wrapping_add(1);
        }
    }

    /// The snapshot's `STATE` arrived: load the staged map.
    fn finish_staging(&mut self, p: usize) {
        let st = &mut self.staging;
        st.pool = usize::MAX;
        let ok = !st.broken
            && st.n == st.expect
            && self.maps[p]
                .commit_stage(st.n, st.lo, st.hi, st.spacing)
                .is_ok();
        if ok {
            self.pools[p].map_ok = true;
            self.pools[p].map_spacing = self.staging.spacing;
            self.counters.maps_loaded = self.counters.maps_loaded.wrapping_add(1);
        } else {
            self.refuse_map(p);
        }
    }

    /// The map and the chain disagree: the pool is not traded (and is
    /// stale in the member's book) until its next snapshot.
    fn refuse_map(&mut self, p: usize) {
        self.pools[p].map_ok = false;
        self.maps[p].clear();
        self.book.mark_stale(self.pools[p].book as usize);
        self.counters.maps_refused = self.counters.maps_refused.wrapping_add(1);
    }

    /// Move pool `p`'s basis EMA toward its current pool-vs-hedge
    /// deviation, time-weighted over `basis_window_ns`. Skipped while our
    /// own swap is in flight (the book carries our impact, which is not
    /// the market's basis) and whenever either price is missing.
    pub(crate) fn refresh_basis(&mut self, p: usize, now: NsTs) {
        let run = self.pools[p];
        if run.inflight_until > now {
            return;
        }
        let b = run.book as usize;
        let (Some(state), Some(meta)) = (self.book.state(b), self.book.pool_meta(b)) else {
            return;
        };
        let pp = self.params.pools[p];
        let (Some(u0), Some(u1)) = (self.usd_mid_1e6(pp.coin0), self.usd_mid_1e6(pp.coin1)) else {
            return;
        };
        let mut pool = core_amm::price_1e18_from_sqrt(
            state.sqrt_price_lo,
            state.sqrt_price_hi,
            meta.dec0,
            meta.dec1,
        );
        // token1 per token0 × 1e18 on the hedge books (u0 ≤ i64::MAX, so
        // the product fits).
        let mut hedge = (u0 as u128) * 1_000_000_000_000_000_000 / (u1 as u128);
        while pool > BASIS_OPERAND_MAX || hedge > BASIS_OPERAND_MAX {
            pool /= 10;
            hedge /= 10;
        }
        if pool == 0 || hedge == 0 {
            return;
        }
        let x = ((pool as i128 - hedge as i128) * (ONE_BPS_1E6 as i128) / (hedge as i128))
            .clamp(-(BASIS_CLAMP_BPS_1E6 as i128), BASIS_CLAMP_BPS_1E6 as i128)
            as i64;
        let r = &mut self.pools[p];
        let w = self.params.basis_window_ns;
        if !r.basis_live {
            r.basis_bps_1e6 = x;
            r.basis_live = true;
        } else {
            let dt = now.saturating_sub(r.basis_ns);
            if dt >= w {
                r.basis_bps_1e6 = x;
            } else {
                let step = ((x - r.basis_bps_1e6) as i128) * (dt as i128) / (w as i128);
                r.basis_bps_1e6 += step as i64;
            }
        }
        if now > r.basis_ns {
            r.basis_ns = now;
        }
    }
}
