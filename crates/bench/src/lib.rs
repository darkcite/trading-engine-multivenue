//! # bench
//!
//! Criterion + dhat benches and the allocation-assertion harness.
//!
//! Phase 0 ships helpers and a smoke test. Real criterion groups are
//! added as the hot path becomes real enough to measure.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use core_parse::scan_price_1e6;

/// Minimal throughput kernel — round-trip N prices through the scanner.
/// Used by both unit tests and later criterion benches to keep the
/// "scanner did not regress" story in one place.
pub fn scan_n_prices(n: usize) -> u64 {
    // A single compile-time-literal price string to avoid any alloc.
    let buf = b"0.518000";
    let mut acc: u64 = 0;
    for _ in 0..n {
        if let Some((v, _)) = scan_price_1e6(buf, 0) {
            acc = acc.wrapping_add(v as u64);
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_n_prices_is_deterministic() {
        let a = scan_n_prices(100);
        let b = scan_n_prices(100);
        assert_eq!(a, b);
    }
}
