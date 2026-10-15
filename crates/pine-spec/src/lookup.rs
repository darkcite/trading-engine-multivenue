// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Name → entry: a CHD minimal perfect hash over FNV-1a 64 (`pine_ir::fnv1a64`), built by
//! `build/mph.rs`, never trusted blind — every hit is confirmed by a byte compare.

use crate::model::Entry;
use pine_ir::{fnv1a64, fnv1a64_continue};

/// The CHD displacement multiplier (golden ratio, 64-bit).
const DISP_MUL: u64 = 0x9E37_79B9_7F4A_7C15;

/// splitmix64's finaliser: every output bit depends on every input bit.
#[inline]
const fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The entry whose MPH slot `h0` (an FNV-1a 64 of the key) lands on, if the slot is occupied.
#[inline]
fn slot_of(h0: u64) -> Option<&'static Entry> {
    let h = h0 ^ crate::MPH_SEED;
    let b = ((mix(h) >> 32) as u32 % crate::MPH_BUCKETS) as usize;
    let d = crate::MPH_DISP[b] as u64;
    let s = (mix(h ^ (d + 1).wrapping_mul(DISP_MUL)) as usize) & (crate::MPH_SLOTS.len() - 1);
    let e = crate::MPH_SLOTS[s];
    if e == u16::MAX {
        None
    } else {
        Some(&crate::ENTRIES[e as usize])
    }
}

/// The entry named by a dotted full name (`"ta.sma"`, `"close"`), or `None`. O(1), no allocation.
#[must_use]
pub fn lookup(full: &str) -> Option<&'static Entry> {
    let e = slot_of(fnv1a64(full.as_bytes()))?;
    if e.full.as_bytes() == full.as_bytes() {
        Some(e)
    } else {
        None
    }
}

/// [`lookup`] of `ns + "." + name` (or of `name` when `ns` is empty) without joining them.
/// `ns` may itself be dotted (`"strategy.risk"`).
#[must_use]
pub fn lookup_parts(ns: &str, name: &str) -> Option<&'static Entry> {
    let (n, m) = (ns.as_bytes(), name.as_bytes());
    let h = if n.is_empty() {
        fnv1a64(m)
    } else {
        fnv1a64_continue(fnv1a64_continue(fnv1a64(n), b"."), m)
    };
    let e = slot_of(h)?;
    let f = e.full.as_bytes();
    let ok = if n.is_empty() {
        f == m
    } else {
        f.len() == n.len() + 1 + m.len()
            && &f[..n.len()] == n
            && f[n.len()] == b'.'
            && &f[n.len() + 1..] == m
    };
    if ok {
        Some(e)
    } else {
        None
    }
}

/// Whether `name` is a spec namespace (`"ta"`, `"strategy.risk"`). Linear over `NAMESPACES`.
#[must_use]
pub fn is_namespace(name: &str) -> bool {
    let b = name.as_bytes();
    let ns = crate::NAMESPACES;
    let mut i = 0usize;
    while i < ns.len() {
        if ns[i].len as usize == b.len() && ns[i].as_bytes() == b {
            return true;
        }
        i += 1;
    }
    false
}
