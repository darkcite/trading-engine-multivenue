// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Value encodings and the `na` model (contract §3.4).
//!
//! | Pine type | lane | `na` in a lane slot | `na` in a storage cell (ring, collection element, object field) |
//! |---|---|---|---|
//! | `float` | f64 | any NaN | any NaN |
//! | `int` | i64 | validity bit clear | [`NA_I64`] |
//! | `bool` v5 | i64 | validity bit clear | [`NA_I64`] |
//! | `bool` v6 | i64 | **never `na`** (W13) | never `na`; rings start `false` |
//! | `color` | i64 | validity bit clear | [`NA_I64`] |
//! | `string` | i64 ([`StrRef`]) | validity bit clear | [`NA_I64`] |
//! | collections, UDT objects | i64 ([`CollRef`]) | validity bit clear | [`NA_I64`] |
//! | enum | i64 (ordinal) | validity bit clear | [`NA_I64`] |
//!
//! The f64 half of the validity bitset exists only so the index arithmetic
//! is one add (I-2); it is initialised to all-ones and **never written**.
//!
//! [`NA_I64`] is never a valid [`StrRef`] (region 2 is reserved) nor a
//! valid [`CollRef`] (bit 63 is always clear), so a storage cell holding it
//! is unambiguous for every i64-lane type except an `int` whose value is
//! genuinely `i64::MIN` — a documented, fixture-pinned limitation.

use crate::ids::SiteId;

/// Canonical float `na`. Any NaN reads as `na`.
pub const NA_F64: f64 = f64::NAN;

/// The i64 `na` sentinel — **storage cells only** (ring cells, collection
/// elements, object fields). Lane slots carry `na` in the validity bitset.
pub const NA_I64: i64 = i64::MIN;

/// [`StrRef`] regions (bits 63..62).
pub mod str_region {
    /// The program's constant-string region: the `SymTable` byte blob.
    /// Immutable and shared by every instance (P1).
    pub const CONST: u64 = 0;
    /// This instance's scratch arena: reset at the top of **every**
    /// evaluation, so a scratch string never outlives the evaluation that
    /// made it.
    pub const SCRATCH: u64 = 1;
    /// Reserved; never produced. Keeps [`crate::NA_I64`] from decoding as
    /// a string.
    pub const RESERVED: u64 = 2;
    /// This instance's persistent cells: one fixed-size cell per persistent
    /// string slot (`var`/`varip` strings) and per string-ring cell. Never
    /// reset; journaled.
    pub const PERSIST: u64 = 3;
}

/// A Pine string value as it sits in an i64 slot or storage cell.
///
/// | bits | field |
/// |---|---|
/// | 63..62 | region ([`str_region`]) |
/// | 61..32 | byte offset within the region (≤ 2^30 − 1) |
/// | 31..0 | byte length |
///
/// `na` is the slot's validity bit (or [`NA_I64`] in a storage cell); a
/// `StrRef` is never itself a sentinel. Equality of two strings is equality
/// of their **bytes**, never of their `StrRef`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct StrRef(pub u64);

impl StrRef {
    /// Largest encodable offset.
    pub const MAX_OFF: u32 = (1 << 30) - 1;

    /// Encode. `region` must be one of the three producible regions and
    /// `off <= MAX_OFF` (debug-asserted; release masks).
    #[inline]
    #[must_use]
    pub const fn new(region: u64, off: u32, len: u32) -> Self {
        debug_assert!(region != str_region::RESERVED && region <= 3);
        debug_assert!(off <= Self::MAX_OFF);
        StrRef(((region & 3) << 62) | (((off & Self::MAX_OFF) as u64) << 32) | len as u64)
    }

    /// A view into the constant region.
    #[inline]
    #[must_use]
    pub const fn konst(off: u32, len: u32) -> Self {
        Self::new(str_region::CONST, off, len)
    }

    /// A scratch-arena string.
    #[inline]
    #[must_use]
    pub const fn scratch(off: u32, len: u32) -> Self {
        Self::new(str_region::SCRATCH, off, len)
    }

    /// A persistent-cell string.
    #[inline]
    #[must_use]
    pub const fn persist(off: u32, len: u32) -> Self {
        Self::new(str_region::PERSIST, off, len)
    }

    /// The region tag.
    #[inline]
    #[must_use]
    pub const fn region(self) -> u64 {
        self.0 >> 62
    }

    /// Byte offset within the region.
    #[inline]
    #[must_use]
    pub const fn off(self) -> u32 {
        ((self.0 >> 32) as u32) & Self::MAX_OFF
    }

    /// Byte length.
    #[inline]
    #[must_use]
    pub const fn len(self) -> u32 {
        self.0 as u32
    }

    /// `len() == 0`.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// False for the reserved region — i.e. for [`NA_I64`] read back from a
    /// storage cell.
    #[inline]
    #[must_use]
    pub const fn is_valid_encoding(self) -> bool {
        self.region() != str_region::RESERVED
    }
}

/// A reference to a collection or UDT object block in the pool.
///
/// | bits | field |
/// |---|---|
/// | 63 | always 0 (keeps [`NA_I64`] invalid) |
/// | 62..32 | generation (31 bits, never 0 for a live block) |
/// | 31..16 | reserved, 0 |
/// | 15..0 | owning site: the [`SiteId`] whose `StateDesc` (kind `Collection` or `Object`) holds the block |
///
/// Pine collections and objects are references: `b = a` copies the
/// `CollRef`, not the block. Re-executing the creating call site bumps the
/// block's generation, so a reference kept from an earlier creation is
/// **stale** and faults as `ScriptRefused` instead of silently aliasing the
/// new object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct CollRef(pub u64);

impl CollRef {
    /// Generation mask (31 bits).
    pub const GEN_MASK: u32 = 0x7FFF_FFFF;

    /// Encode. `gen` is masked to 31 bits and must be non-zero for a live
    /// block (debug-asserted).
    #[inline]
    #[must_use]
    pub const fn new(site: SiteId, gen: u32) -> Self {
        debug_assert!(gen & Self::GEN_MASK != 0);
        CollRef((((gen & Self::GEN_MASK) as u64) << 32) | site.0 as u64)
    }

    /// The owning site.
    #[inline]
    #[must_use]
    pub const fn site(self) -> SiteId {
        SiteId(self.0 as u16)
    }

    /// The generation.
    #[inline]
    #[must_use]
    pub const fn gen(self) -> u32 {
        ((self.0 >> 32) as u32) & Self::GEN_MASK
    }

    /// False for [`NA_I64`] and for anything with reserved bits set.
    #[inline]
    #[must_use]
    pub const fn is_valid_encoding(self) -> bool {
        (self.0 >> 63) == 0 && (self.0 >> 16) & 0xFFFF == 0 && self.gen() != 0
    }

    /// The next generation after `gen`: 31-bit wrap that skips 0.
    #[inline]
    #[must_use]
    pub const fn next_gen(gen: u32) -> u32 {
        let n = gen.wrapping_add(1) & Self::GEN_MASK;
        if n == 0 {
            1
        } else {
            n
        }
    }
}

/// What a pool block holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CollKind {
    /// `array<T>`.
    Array = 0,
    /// `map<K, V>`.
    Map = 1,
    /// `matrix<T>` (Tier 4, deferred).
    Matrix = 2,
    /// A UDT object: `len` fields of 8 bytes each.
    Object = 3,
}

/// Header at the start of every pool block (collection or UDT object).
/// 32 bytes; the block's element area starts at `data_off`.
///
/// Owned by `pine-std` for collections and by `pine-vm` for objects; both
/// read it through `pine-vm`'s `Frame`, which journals the whole block on
/// the first mutable touch of a tick (contract §3.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct CollHeader {
    /// Bumped on every (re)initialisation; 0 = never initialised. A
    /// [`CollRef`] whose `gen` differs is stale.
    pub gen: u32,
    /// Element capacity of the backing storage (fixed at layout).
    pub cap: u32,
    /// Live element count (fields, for an object).
    pub len: u32,
    /// Frame-relative byte offset of element 0. 8-aligned.
    pub data_off: u32,
    /// [`CollKind`] as a raw byte.
    pub kind: u8,
    /// Element (array, matrix) or value (map) type: [`crate::Ty`] as a raw
    /// byte; 0 for an object.
    pub elem: u8,
    /// Map key type ([`crate::Ty`] raw); 0 otherwise.
    pub key: u8,
    /// 0 = owner; 1 = a view (`array.slice`) onto `view_of`'s elements.
    pub view: u8,
    /// Maps: frame-relative byte offset of the open-addressing index.
    pub idx_off: u32,
    /// Maps: index capacity − 1 (power of two).
    pub idx_mask: u32,
    /// Views: the owning block's site; `u16::MAX` otherwise.
    pub view_of: u16,
    /// Always zero.
    pub _pad: u16,
}

/// Colour packing in an i64 slot (upper 32 bits zero):
/// bits 0..7 blue, 8..15 green, 16..23 red, 24..31 **transparency**
/// `0..=100` (Pine's `color.new(c, transp)` / `color.t(c)` convention).
pub mod color {
    /// Pack red, green, blue and transparency (`0..=100`, clamped).
    #[inline]
    #[must_use]
    pub const fn pack(r: u8, g: u8, b: u8, transp: u8) -> i64 {
        let t = if transp > 100 { 100 } else { transp };
        ((t as i64) << 24) | ((r as i64) << 16) | ((g as i64) << 8) | b as i64
    }

    /// Red.
    #[inline]
    #[must_use]
    pub const fn r(c: i64) -> u8 {
        (c >> 16) as u8
    }

    /// Green.
    #[inline]
    #[must_use]
    pub const fn g(c: i64) -> u8 {
        (c >> 8) as u8
    }

    /// Blue.
    #[inline]
    #[must_use]
    pub const fn b(c: i64) -> u8 {
        c as u8
    }

    /// Transparency `0..=100`.
    #[inline]
    #[must_use]
    pub const fn t(c: i64) -> u8 {
        (c >> 24) as u8
    }
}

const _: () = assert!(core::mem::size_of::<StrRef>() == 8);
const _: () = assert!(core::mem::size_of::<CollRef>() == 8);
const _: () = assert!(core::mem::size_of::<CollKind>() == 1);
const _: () = assert!(core::mem::size_of::<CollHeader>() == 32);
const _: () = assert!(core::mem::align_of::<CollHeader>() == 4);
// NA_I64 decodes as neither a string nor a collection.
const _: () = assert!(!StrRef(NA_I64 as u64).is_valid_encoding());
const _: () = assert!(!CollRef(NA_I64 as u64).is_valid_encoding());
