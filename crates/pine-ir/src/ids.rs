// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Identifiers, spans and the dialect tag (contract §3.1).

/// An interned string in one compilation's [`crate::SymTable`].
///
/// Interning is per **compilation**, not per `Ast`: `pine-lang` interns
/// into a table the caller owns and passes in, `pine-sema` adds to the same
/// table while resolving imports, and the finished table moves into the
/// `PineProgram`. A `Sym` is therefore comparable everywhere inside one
/// program and meaningless across programs — cross-program keys are
/// [`crate::symbol_hash`] and [`crate::str_key`], never a `Sym`.
///
/// `Sym(0)` is reserved and means "none"; it resolves to the empty string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct Sym(pub u32);

impl Sym {
    /// The reserved "no symbol" value. Resolves to `""`.
    pub const NONE: Sym = Sym(0);
}

/// An AST node index inside one `pine_lang::Ast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct NodeId(pub u32);

impl NodeId {
    /// "No node" — e.g. the call site of a program's root instantiation.
    pub const NONE: NodeId = NodeId(u32::MAX);
}

/// A stateful call site, after monomorphisation. Dense per program:
/// `FrameLayout::states[i].site == SiteId(i)` (invariant I-4), so a site
/// id is also its state-descriptor index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct SiteId(pub u16);

impl SiteId {
    /// "Not stateful". Never assigned to a real site; a stateless builtin is
    /// called with this value and must not dereference it.
    pub const NONE: SiteId = SiteId(u16::MAX);
}

/// A source file of one compilation. `FileId(0)` is the root script;
/// imported libraries get `1..n` in resolution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct FileId(pub u16);

impl FileId {
    /// The root script.
    pub const ROOT: FileId = FileId(0);
}

/// Index of one builtin **overload** in `pine_spec::OVERLOADS`.
///
/// The builtin table `pine-std` exports is indexed by this id, not by the
/// entry id: two overloads of one name (`math.max(float…)`,
/// `math.max(int…)`) are two monomorphic bodies and two table rows. The
/// numbering is only meaningful together with the spec table it came from,
/// which is why `ProgramMeta::spec_sha256` and `BuiltinTable::spec_sha256`
/// must agree before an instance may be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct OverloadId(pub u16);

/// A byte range in one source file. 12 bytes; `lo..hi` is half-open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Span {
    /// Which file of the compilation.
    pub file: FileId,
    /// Always zero.
    pub _pad: u16,
    /// First byte.
    pub lo: u32,
    /// One past the last byte.
    pub hi: u32,
}

impl Default for FileId {
    #[inline]
    fn default() -> Self {
        FileId::ROOT
    }
}

impl Span {
    /// A span in `file` covering `lo..hi`.
    #[inline]
    #[must_use]
    pub const fn new(file: FileId, lo: u32, hi: u32) -> Self {
        Self { file, _pad: 0, lo, hi }
    }
}

/// The Pine dialect a script declares with `//@version=`. v4 and below are
/// refused at load; there is no third variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Dialect {
    /// Pine Script v5.
    V5 = 5,
    /// Pine Script v6.
    V6 = 6,
}

impl Dialect {
    /// `5 -> V5`, `6 -> V6`, anything else `None` (the v4-refusal path).
    #[inline]
    #[must_use]
    pub const fn from_version(v: u32) -> Option<Self> {
        match v {
            5 => Some(Self::V5),
            6 => Some(Self::V6),
            _ => None,
        }
    }
}

/// A stable 64-bit key of a byte string: [`crate::str_key`] over its bytes.
///
/// Order ids, `from_entry` references and OCA group names are keyed this
/// way, so a constant id and the same id built at runtime by string
/// concatenation are the same key, and a key is comparable across programs
/// and across processes (the determinism tape, the conformance fixtures).
/// `StrKey(0)` is reserved and means "none"; [`crate::str_key`] never
/// returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct StrKey(pub u64);

impl StrKey {
    /// "No key" — an absent id, "all entries", no OCA group.
    pub const NONE: StrKey = StrKey(0);
}

const _: () = assert!(core::mem::size_of::<Sym>() == 4);
const _: () = assert!(core::mem::size_of::<NodeId>() == 4);
const _: () = assert!(core::mem::size_of::<SiteId>() == 2);
const _: () = assert!(core::mem::size_of::<FileId>() == 2);
const _: () = assert!(core::mem::size_of::<OverloadId>() == 2);
const _: () = assert!(core::mem::size_of::<StrKey>() == 8);
const _: () = assert!(core::mem::size_of::<Span>() == 12);
const _: () = assert!(core::mem::align_of::<Span>() == 4);
const _: () = assert!(core::mem::size_of::<Dialect>() == 1);
