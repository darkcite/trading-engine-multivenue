// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The M2 capped-options-universe selection LAW — extracted at M2 close
//! (operator-ruled 2026-08-22; docs/m2-progress.md close entry) from the
//! three verbatim twins that grew under the rule of three:
//! `ingress-deribit::discovery` (the original law source), `ingress-okx::
//! discovery`, `ingress-binance::eapi`. The venue crates keep their public
//! `select_capped_chain` signatures as thin wrappers over [`select_capped_chain`]
//! here, so every existing call site, property test and fuzz target pins the
//! SAME behavior through the venue surface it always used.
//!
//! DOCTRINE: boot-only, offline path — this module allocates freely
//! (`Vec` growth) and is never on the hot path. No unsafe, no deps.
//!
//! The law (M2 Session-0 design entry; mvp-plan §4-M2 / options-plan §2):
//!
//! - candidates: whatever the venue's `eligible` predicate admits (each
//!   venue owns its candidacy test — option-kind + live + unexpired, plus
//!   the eapi underlying filter);
//! - the nearest `expiries_e` DISTINCT expiries, ascending;
//! - per expiry, the `strikes_k` strikes nearest ATM, POSITION-BASED: the
//!   last K/2 distinct strikes at-or-below `index_px_1e9` + the first K/2
//!   above — no distance tie-breaks, and a short side is NOT backfilled
//!   from the other side (the cap is a maximum, not a promise);
//! - both calls and puts at every selected (expiry, strike) — a missing
//!   twin simply isn't emitted.
//!
//! Output order is the DETERMINISTIC allocation order the cli assigns
//! options ordinals in: expiry asc → strike asc → call before put.
//! Output length ≤ `E × K × 2` by construction.

/// The row view the selection law needs from a venue discovery row.
/// Implemented by `DeribitInstrumentRow`, `OkxInstrumentRow`,
/// `EapiOptionRow` in their own crates. `Copy` because selected rows are
/// returned by value (POD discovery rows, house rule).
pub trait ChainRow: Copy {
    /// Expiry, unix ms.
    fn exp_ms(&self) -> i64;
    /// Strike ×1e9.
    fn strike_1e9(&self) -> i64;
    /// Call (true) / put (false).
    fn is_call(&self) -> bool;
}

/// Apply the capped universe policy to ONE underlying's parsed option
/// rows. `eligible` is the venue candidacy predicate (kind/live/expiry
/// checks live there — the law never re-tests them beyond grouping).
/// Precondition: `rows` is one underlying's page, ingested once (venues
/// list each instrument once). Boot-only: allocates freely.
pub fn select_capped_chain<R: ChainRow, F: Fn(&R) -> bool>(
    rows: &[R],
    eligible: F,
    index_px_1e9: i64,
    expiries_e: u32,
    strikes_k: u32,
) -> Vec<R> {
    let mut out: Vec<R> = Vec::new();
    if expiries_e == 0 || strikes_k == 0 {
        return out;
    }

    // Distinct eligible expiries, ascending, nearest E kept.
    let mut expiries: Vec<i64> = Vec::new();
    for r in rows {
        if eligible(r) && !expiries.contains(&r.exp_ms()) {
            expiries.push(r.exp_ms());
        }
    }
    expiries.sort_unstable();
    expiries.truncate(expiries_e as usize);

    let half = (strikes_k / 2) as usize;
    for &exp in &expiries {
        // Distinct strikes at this expiry, ascending.
        let mut strikes: Vec<i64> = Vec::new();
        for r in rows {
            if eligible(r) && r.exp_ms() == exp && !strikes.contains(&r.strike_1e9()) {
                strikes.push(r.strike_1e9());
            }
        }
        strikes.sort_unstable();
        // Position-based ATM split: last `half` at-or-below + first
        // `half` above the index.
        let below_end = strikes.partition_point(|&s| s <= index_px_1e9);
        let lo = below_end.saturating_sub(half);
        let hi = (below_end + half).min(strikes.len());
        for &strike in &strikes[lo..hi] {
            // Call before put, at most one row each (venue lists each
            // instrument once — precondition above).
            for want_call in [true, false] {
                for r in rows {
                    if eligible(r)
                        && r.exp_ms() == exp
                        && r.strike_1e9() == strike
                        && r.is_call() == want_call
                    {
                        out.push(*r);
                        break;
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    struct Row {
        exp: i64,
        strike: i64,
        call: bool,
        live: bool,
    }

    impl ChainRow for Row {
        fn exp_ms(&self) -> i64 {
            self.exp
        }
        fn strike_1e9(&self) -> i64 {
            self.strike
        }
        fn is_call(&self) -> bool {
            self.call
        }
    }

    const NOW: i64 = 1_000;

    fn elig(r: &Row) -> bool {
        r.live && r.exp > NOW
    }

    fn grid(expiries: &[i64], strikes: &[i64]) -> Vec<Row> {
        let mut v = Vec::new();
        for &exp in expiries {
            for &strike in strikes {
                for call in [true, false] {
                    v.push(Row { exp, strike, call, live: true });
                }
            }
        }
        v
    }

    #[test]
    fn happy_path_order_and_cap() {
        // 3 expiries × 4 strikes; E=2, K=2 around index 25 → strikes {20,30}.
        let rows = grid(&[2_000, 3_000, 4_000], &[10, 20, 30, 40]);
        let sel = select_capped_chain(&rows, elig, 25, 2, 2);
        // ≤ E×K×2 = 8, exactly 8 on a full grid.
        assert_eq!(sel.len(), 8);
        // Deterministic order: expiry asc → strike asc → call before put.
        let want: Vec<(i64, i64, bool)> = vec![
            (2_000, 20, true),
            (2_000, 20, false),
            (2_000, 30, true),
            (2_000, 30, false),
            (3_000, 20, true),
            (3_000, 20, false),
            (3_000, 30, true),
            (3_000, 30, false),
        ];
        let got: Vec<(i64, i64, bool)> = sel.iter().map(|r| (r.exp, r.strike, r.call)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn zero_e_or_k_is_empty() {
        let rows = grid(&[2_000], &[10]);
        assert!(select_capped_chain(&rows, elig, 10, 0, 8).is_empty());
        assert!(select_capped_chain(&rows, elig, 10, 2, 0).is_empty());
    }

    #[test]
    fn expired_and_ineligible_rows_never_selected() {
        let mut rows = grid(&[500, 2_000], &[10, 20]);
        // Kill liveness on one in-date row too.
        rows.push(Row { exp: 2_000, strike: 30, call: true, live: false });
        let sel = select_capped_chain(&rows, elig, 15, 4, 32);
        assert!(sel.iter().all(|r| r.exp == 2_000 && r.live && r.strike != 30));
    }

    #[test]
    fn one_sided_ladder_is_not_backfilled() {
        // All strikes above the index: the at-or-below side is empty and
        // stays empty — only the first K/2 above are taken.
        let rows = grid(&[2_000], &[100, 200, 300, 400]);
        let sel = select_capped_chain(&rows, elig, 50, 1, 4);
        let strikes: Vec<i64> = sel.iter().map(|r| r.strike).collect();
        assert_eq!(strikes, vec![100, 100, 200, 200]);
    }

    #[test]
    fn missing_twin_is_not_emitted() {
        let mut rows = grid(&[2_000], &[10]);
        // Remove the put at (2_000, 10).
        rows.retain(|r| r.call);
        let sel = select_capped_chain(&rows, elig, 10, 1, 2);
        assert_eq!(sel.len(), 1);
        assert!(sel[0].call);
    }

    #[test]
    fn position_split_prefers_at_or_below_boundary() {
        // Index exactly ON a strike: that strike counts as at-or-below.
        let rows = grid(&[2_000], &[10, 20, 30, 40]);
        let sel = select_capped_chain(&rows, elig, 20, 1, 2);
        let strikes: Vec<i64> = sel.iter().map(|r| r.strike).collect();
        assert_eq!(strikes, vec![20, 20, 30, 30]);
    }
}
