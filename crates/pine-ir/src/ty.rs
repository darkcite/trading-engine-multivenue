// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Types and qualifiers (contract §3.2).

/// A Pine type. `Void` occupies no slot; `Float` lives in the f64 lane;
/// everything else lives in the i64 lane (contract §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Ty {
    /// No value.
    #[default]
    Void = 0,
    /// `int` — i64 lane.
    Int = 1,
    /// `float` — f64 lane; `na` is NaN.
    Float = 2,
    /// `bool` — i64 lane, 0/1. In v6 a bool is never `na` (W13).
    Bool = 3,
    /// `color` — i64 lane, [`crate::color`] packing.
    Color = 4,
    /// `string` — i64 lane, a [`crate::StrRef`].
    String = 5,
    /// `array<T>` — i64 lane, a [`crate::CollRef`].
    Array = 6,
    /// `map<K, V>` — i64 lane, a [`crate::CollRef`].
    Map = 7,
    /// `matrix<T>` — i64 lane, a [`crate::CollRef`] (Tier 4, deferred).
    Matrix = 8,
    /// A user-defined type — i64 lane, a [`crate::CollRef`] to an object block.
    Udt = 9,
    /// An enum (v6) — i64 lane, the member ordinal.
    Enum = 10,
    /// A `plot` handle (what `plot()` returns, `fill()` takes) — i64 lane, the plot ordinal
    /// (amendment A4.2).
    Plot = 11,
    /// An `hline` handle (what `hline()` returns, `fill()` takes) — i64 lane, the hline ordinal
    /// (amendment A4.2).
    Hline = 12,
}

impl Ty {
    /// Decode a raw byte. `None` for anything unassigned.
    #[inline]
    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Void),
            1 => Some(Self::Int),
            2 => Some(Self::Float),
            3 => Some(Self::Bool),
            4 => Some(Self::Color),
            5 => Some(Self::String),
            6 => Some(Self::Array),
            7 => Some(Self::Map),
            8 => Some(Self::Matrix),
            9 => Some(Self::Udt),
            10 => Some(Self::Enum),
            11 => Some(Self::Plot),
            12 => Some(Self::Hline),
            _ => None,
        }
    }

    /// The lane a value of this type occupies; `None` for `Void`.
    #[inline]
    #[must_use]
    pub const fn lane(self) -> Option<crate::Lane> {
        match self {
            Self::Void => None,
            Self::Float => Some(crate::Lane::F64),
            _ => Some(crate::Lane::I64),
        }
    }
}

/// The qualifier lattice. Ordering is meaningful:
/// `Const < Input < Simple < Series`, and a join is `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Qual {
    /// Known at compile time.
    Const = 0,
    /// Fixed for the life of an instance; folded at load from the manifest.
    Input = 1,
    /// Fixed for the life of an instance; known on the first bar.
    Simple = 2,
    /// May change every bar.
    Series = 3,
}

/// A type with its qualifier. 8 bytes.
///
/// `a` and `b` are interpreted by `ty`, and there is **no side table**
/// (v4, review row 34): nested collections are refused at load, so an
/// element is always a scalar, an enum or a UDT, and fits in one
/// [`elem`]-encoded `u16`:
///
/// | `ty` | `a` | `b` |
/// |---|---|---|
/// | scalars, `Void` | 0 | 0 |
/// | `Array`, `Matrix` | element, [`elem`] encoding | 0 |
/// | `Map` | key, [`elem`] encoding (scalar or enum only) | value, [`elem`] encoding |
/// | `Udt` | UDT declaration index | 0 |
/// | `Enum` | enum declaration index | 0 |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct TypeQ {
    /// The type.
    pub ty: Ty,
    /// The qualifier.
    pub qual: Qual,
    /// First type argument (see the table above).
    pub a: u16,
    /// Second type argument (see the table above).
    pub b: u16,
    /// Always zero.
    pub _pad: u16,
}

impl TypeQ {
    /// A scalar (or `Void`) type.
    #[inline]
    #[must_use]
    pub const fn scalar(ty: Ty, qual: Qual) -> Self {
        Self {
            ty,
            qual,
            a: 0,
            b: 0,
            _pad: 0,
        }
    }

    /// `array<elem>`; `elem` is an [`elem`]-encoded element.
    #[inline]
    #[must_use]
    pub const fn array(elem: u16, qual: Qual) -> Self {
        Self {
            ty: Ty::Array,
            qual,
            a: elem,
            b: 0,
            _pad: 0,
        }
    }

    /// `map<key, value>`; both [`elem`]-encoded.
    #[inline]
    #[must_use]
    pub const fn map(key: u16, value: u16, qual: Qual) -> Self {
        Self {
            ty: Ty::Map,
            qual,
            a: key,
            b: value,
            _pad: 0,
        }
    }

    /// The same type with a different qualifier.
    #[inline]
    #[must_use]
    pub const fn with_qual(self, qual: Qual) -> Self {
        Self {
            ty: self.ty,
            qual,
            a: self.a,
            b: self.b,
            _pad: 0,
        }
    }
}

/// The `u16` element encoding used by [`TypeQ::a`] / [`TypeQ::b`] for
/// collection element, key and value types.
pub mod elem {
    use super::Ty;

    /// Bit 15 set: a scalar; the low byte is the [`Ty`].
    pub const SCALAR: u16 = 0x8000;
    /// Bit 14 set (bit 15 clear): an enum; bits 0..14 are its declaration index.
    pub const ENUM: u16 = 0x4000;
    /// Neither bit: a UDT; bits 0..14 are its declaration index.
    pub const DECL_MASK: u16 = 0x3FFF;

    /// Encode a scalar element (`Int`, `Float`, `Bool`, `Color`, `String`).
    #[inline]
    #[must_use]
    pub const fn scalar(ty: Ty) -> u16 {
        SCALAR | ty as u16
    }

    /// Encode an enum element by declaration index (`< 0x4000`).
    #[inline]
    #[must_use]
    pub const fn enumeration(decl: u16) -> u16 {
        ENUM | (decl & DECL_MASK)
    }

    /// Encode a UDT element by declaration index (`< 0x4000`).
    #[inline]
    #[must_use]
    pub const fn udt(decl: u16) -> u16 {
        decl & DECL_MASK
    }

    /// The scalar type of an encoded element, or `None` when it is an enum
    /// or a UDT.
    #[inline]
    #[must_use]
    pub const fn as_scalar(e: u16) -> Option<Ty> {
        if e & SCALAR != 0 {
            Ty::from_u8((e & 0xFF) as u8)
        } else {
            None
        }
    }
}

const _: () = assert!(core::mem::size_of::<Ty>() == 1);
const _: () = assert!(core::mem::size_of::<Qual>() == 1);
const _: () = assert!(core::mem::size_of::<TypeQ>() == 8);
const _: () = assert!(core::mem::align_of::<TypeQ>() == 2);
