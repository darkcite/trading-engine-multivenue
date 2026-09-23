// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The pool table: every subscribed pool's address → its `SymbolId`
//! (spec-gap default G3 — each pool is registered as a symbol, so a
//! pool event travels as an ordinary `Signal` addressed by `sym`).
//!
//! Built once at boot, sorted by address; the per-log lookup is a binary
//! search over a fixed array. The `0x…` ASCII form of every address is
//! rendered here once, so the subscribe frame never formats an address.

use core_types::SymbolId;

use crate::hex::render_hex;

/// Most pools one ingress subscribes to.
pub const HYPEREVM_MAX_POOLS: usize = 128;

/// Which contract family a pool is — decides its discovery reads, its
/// decoders and its swap loop (`core_amm::AMM_KIND_*`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PoolFamily {
    /// Uniswap V3 ABI: 7-word `slot0()`, 8-word `ticks()`.
    UniswapV3 = 0,
    /// Aerodrome Slipstream fork (Hybra CL): 6-word `slot0()`, 10-word
    /// `ticks()`, same loop and math as V3.
    Slipstream = 1,
    /// Algebra Integral (v1.0 / v1.2): `globalState()`, 6-word `ticks()`
    /// with a linked list, the Algebra loop.
    Algebra = 2,
}

/// Why a pool table was refused (boot).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PoolTableErr {
    /// More than [`HYPEREVM_MAX_POOLS`] pools.
    TooMany,
    /// The same address twice.
    Duplicate,
    /// No pools.
    Empty,
}

/// One pool at boot: address, symbol, family.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PoolEntry {
    /// Pool contract address.
    pub address: [u8; 20],
    /// The pool's registered symbol.
    pub sym: SymbolId,
    /// Contract family.
    pub family: PoolFamily,
}

/// Sorted, fixed-capacity pool table.
pub struct PoolTable {
    n: usize,
    entries: [PoolEntry; HYPEREVM_MAX_POOLS],
    hex: [[u8; 42]; HYPEREVM_MAX_POOLS],
}

impl PoolTable {
    /// Build from the boot list (any order). Boot-time only.
    pub fn new(pools: &[PoolEntry]) -> Result<Self, PoolTableErr> {
        if pools.is_empty() {
            return Err(PoolTableErr::Empty);
        }
        if pools.len() > HYPEREVM_MAX_POOLS {
            return Err(PoolTableErr::TooMany);
        }
        let blank = PoolEntry {
            address: [0; 20],
            sym: 0,
            family: PoolFamily::UniswapV3,
        };
        let mut t = Self {
            n: pools.len(),
            entries: [blank; HYPEREVM_MAX_POOLS],
            hex: [[0u8; 42]; HYPEREVM_MAX_POOLS],
        };
        // COPY: the boot pool list (≤ 128 × 28 B) into the table it is sorted in — boot only, once.
        t.entries[..pools.len()].copy_from_slice(pools);
        t.entries[..pools.len()].sort_unstable_by_key(|e| e.address);
        let mut i = 0;
        while i < t.n {
            if i > 0 && t.entries[i].address == t.entries[i - 1].address {
                return Err(PoolTableErr::Duplicate);
            }
            let w = render_hex(&mut t.hex[i], &t.entries[i].address);
            debug_assert_eq!(w, Some(42));
            i += 1;
        }
        Ok(t)
    }

    /// Number of pools.
    #[inline]
    pub fn len(&self) -> usize {
        self.n
    }

    /// Never true for a built table (an empty list is refused).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The pools, sorted by address.
    #[inline]
    pub fn entries(&self) -> &[PoolEntry] {
        &self.entries[..self.n]
    }

    /// The `0x…` lowercase ASCII address of every pool, same order.
    #[inline]
    pub fn addresses_hex(&self) -> &[[u8; 42]] {
        &self.hex[..self.n]
    }

    /// The pool at `address`, if subscribed. Binary search, no alloc.
    #[inline]
    pub fn lookup(&self, address: &[u8; 20]) -> Option<&PoolEntry> {
        let mut lo = 0usize;
        let mut hi = self.n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let m = &self.entries[mid];
            match m.address.cmp(address) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(m),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(b: u8, sym: SymbolId) -> PoolEntry {
        PoolEntry {
            address: [b; 20],
            sym,
            family: PoolFamily::UniswapV3,
        }
    }

    #[test]
    fn sorted_lookup_and_rendered_addresses() {
        let t = PoolTable::new(&[e(0x30, 3), e(0x10, 1), e(0x20, 2)]).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t.lookup(&[0x20; 20]).map(|p| p.sym), Some(2));
        assert_eq!(t.lookup(&[0x10; 20]).map(|p| p.sym), Some(1));
        assert_eq!(t.lookup(&[0x31; 20]), None);
        assert_eq!(&t.addresses_hex()[0][..6], b"0x1010");
        assert_eq!(t.entries()[2].sym, 3);
    }

    #[test]
    fn refuses_bad_tables() {
        assert_eq!(PoolTable::new(&[]).err(), Some(PoolTableErr::Empty));
        assert_eq!(
            PoolTable::new(&[e(1, 1), e(1, 2)]).err(),
            Some(PoolTableErr::Duplicate)
        );
        let many = [e(0, 0); HYPEREVM_MAX_POOLS + 1];
        assert_eq!(PoolTable::new(&many).err(), Some(PoolTableErr::TooMany));
    }
}
