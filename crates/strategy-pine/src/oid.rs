// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.5.3 (T12): the engine client oid, `epoch << 32 | script << 29 | oid` (I25).
//!
//! Hot module (I4): `const fn`s, no allocation, no clock.

/// Bits of the broker's own oid in the engine client oid.
pub const BROKER_OID_BITS: u32 = 29;
/// Bits of the script index.
pub const SCRIPT_BITS: u32 = 3;

// Eight scripts fit the script bits (`MAX_SCRIPTS` is asserted a power of two in `lib.rs`).
const _: () = assert!(crate::MAX_SCRIPTS == 1 << SCRIPT_BITS);
const _: () = assert!(BROKER_OID_BITS + SCRIPT_BITS == 32);

/// The broker oid mask.
const OID_MASK: u32 = (1 << BROKER_OID_BITS) - 1;

/// `epoch << 32 | script << 29 | oid`. `epoch ≠ 0`, `script < 8`, `1 ≤ oid < 2^29`
/// (`debug_assert!`; an oid past 2^29 − 1 is our defect, unreachable at any issue rate a boot can
/// reach).
#[inline(always)]
#[must_use]
pub const fn encode_client_oid(epoch: u32, script: u8, oid: u32) -> u64 {
    debug_assert!(epoch != 0);
    debug_assert!((script as usize) < crate::MAX_SCRIPTS);
    debug_assert!(oid != 0 && oid <= OID_MASK);
    ((epoch as u64) << 32) | ((script as u64) << BROKER_OID_BITS) | ((oid & OID_MASK) as u64)
}

/// `Some((script, oid))` iff the epoch matches this boot's and `oid ≠ 0`.
#[inline(always)]
#[must_use]
#[allow(clippy::cast_possible_truncation)] // both halves are masked to 32 bits first
pub const fn decode_client_oid(epoch: u32, client_oid: u64) -> Option<(u8, u32)> {
    let hi = (client_oid >> 32) as u32;
    let lo = (client_oid & 0xFFFF_FFFF) as u32;
    let oid = lo & OID_MASK;
    if hi != epoch || epoch == 0 || oid == 0 {
        return None;
    }
    Some(((lo >> BROKER_OID_BITS) as u8, oid))
}
