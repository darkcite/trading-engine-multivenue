//! # core-simd
//!
//! Feature-detected SIMD kernels. Compile-time dispatched per target:
//! AVX2 on x86_64, NEON on aarch64, scalar fallback elsewhere.
//!
//! Kernels live here so each ingress parser can import
//! `core_simd::parse_price_1e6` without caring which ISA is under it.
//! The scalar fallback is correct; the SIMD paths exist to beat it.
//!
//! ## Scaffold status
//!
//! The scalar fallback is the only kernel implemented in Phase 0.
//! AVX2/NEON versions land in Phase 3 under `simd::{avx2,neon}` once
//! there is a real benchmark showing a win over the scalar path.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

/// Sum of bytes in `buf` — a cheap kernel used as a compile-target
/// check and a stand-in for later SIMD work. The scalar impl is kept
/// deliberately simple; AVX2/NEON variants will live behind cfg gates
/// and be dispatched at compile time by their callers.
#[inline]
pub fn byte_sum(buf: &[u8]) -> u64 {
    let mut s: u64 = 0;
    // Raw-index loop so LLVM auto-vectorises cleanly (no iterator adapter).
    let mut i = 0;
    while i < buf.len() {
        s = s.wrapping_add(buf[i] as u64);
        i += 1;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sum_of_empty_is_zero() {
        assert_eq!(byte_sum(b""), 0);
    }

    #[test]
    fn byte_sum_of_abc() {
        // Cast each byte up to u64 *before* the adds to avoid u8 overflow.
        assert_eq!(byte_sum(b"abc"), b'a' as u64 + b'b' as u64 + b'c' as u64);
    }
}
