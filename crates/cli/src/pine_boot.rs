// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The Pine member's (slot 8) host seam in the `run` boot (impl-08 §3.6, `pine_boot.rs`).
//! Boot-only; may allocate.
//!
//! Until impl-08 T19 lands `--pine <manifest>` and `set.pine_mut().configure(..)`, this binary
//! cannot configure slot 8. A `--strategy` that names `pine` would otherwise be dropped
//! silently: `paper::engine_loop_set_full` boots `requested & configured`, and no flag ever sets
//! slot 8's configured bit — so `ai+pine` would boot as `ai`. F19 (requested-but-absent
//! REFUSES, the icdp law): the run arm refuses such a boot FIRST, before the config load, the
//! ingress threads, the exec artifact and every live or gateway arm (risk-policy "PVM — slot 8,
//! paper only").

use core_types::SlotMask;

/// The refusal the `run` arm logs before it exits 1.
pub const PINE_UNCONFIGURABLE: &str = "pine: requested by --strategy but this binary cannot \
     configure it (impl-08 T19 --pine) — boot aborted";

/// `Err` when `requested` (a `strategy_set::parse_strategy` mask) names slot 8, which this
/// binary cannot configure; `Ok` otherwise. Fail closed: the bit alone decides.
pub fn check_requested(requested: SlotMask) -> Result<(), &'static str> {
    if requested & strategy_set::BIT_PINE != 0 {
        return Err(PINE_UNCONFIGURABLE);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_requested_pine_refuses_and_nothing_else_does() {
        for s in [
            "pine",
            "ai+pine",
            "pine+bin15",
            "ai+vrp+xsd+bin15+hyparb+xmm+hcv+pine",
        ] {
            let m = strategy_set::parse_strategy(s).unwrap();
            assert_eq!(check_requested(m), Err(PINE_UNCONFIGURABLE), "{s}");
        }
        for s in ["all", "ai", "ai+vrp+xsd+bin15+hyparb+xmm+hcv", "hcv", "vm"] {
            let m = strategy_set::parse_strategy(s).unwrap();
            assert_eq!(check_requested(m), Ok(()), "{s}");
        }
        // Fail closed on the bit alone, whatever else the mask holds.
        assert!(check_requested(SlotMask::MAX).is_err());
        assert!(check_requested(strategy_set::BIT_PINE).is_err());
    }
}
