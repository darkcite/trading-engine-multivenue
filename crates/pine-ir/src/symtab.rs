// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The per-compilation interner (contract §3.1).
//!
//! COPY-DOCTRINE: compile-time only. `intern` copies each distinct string
//! into the table once, at load; the runtime side (`resolve`, `bytes`,
//! `str_at`) borrows and never copies.

use crate::hash::fnv1a64;
use crate::ids::Sym;

/// Append-only during compilation; immutable, `Send + Sync` and shared by
/// every instance afterwards (law P1).
///
/// The caller owns the table: `pine_lang::parse` interns into it,
/// `pine_sema::compile` keeps adding while it resolves imports, and the
/// finished table moves into `PineProgram::syms`. `Sym(0)` is pre-interned
/// as the empty string.
///
/// The concatenated byte blob ([`SymTable::bytes`]) doubles as the
/// program's constant-string region: a `StrRef` in the `CONST` region is an
/// `(offset, len)` into it, so a substring of an interned literal is a view,
/// not a copy.
#[derive(Debug, Clone)]
pub struct SymTable {
    /// Every interned string, back to back.
    bytes: Vec<u8>,
    /// `ends[i]` is one past the last byte of `Sym(i)`; `Sym(i)` starts at
    /// `ends[i - 1]` (or 0).
    ends: Vec<u32>,
    /// Open-addressed index: `Sym` value + 1, or 0 for an empty bucket.
    /// Power-of-two length, load factor ≤ ½. Deterministic (no random
    /// state), so two compilations of one source intern identically.
    index: Vec<u32>,
}

impl Default for SymTable {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl SymTable {
    /// Minimum index capacity.
    const MIN_INDEX: usize = 64;

    /// An empty table holding only `Sym(0) == ""`.
    #[must_use]
    pub fn new() -> Self {
        let mut t = Self { bytes: Vec::new(), ends: Vec::new(), index: vec![0; Self::MIN_INDEX] };
        t.ends.push(0);
        let h = fnv1a64(b"") as usize & (Self::MIN_INDEX - 1);
        t.index[h] = 1;
        t
    }

    /// Intern `s`, returning its existing symbol when already present.
    /// Compile time only. Panics only on a table past `u32::MAX` bytes or
    /// symbols, which no script reaches.
    pub fn intern(&mut self, s: &str) -> Sym {
        if let Some(found) = self.get(s) {
            return found;
        }
        if (self.ends.len() + 1) * 2 > self.index.len() {
            self.grow();
        }
        let id = u32::try_from(self.ends.len()).expect("SymTable: more than u32::MAX symbols");
        // COPY: the interned string into the table, once per distinct string, at compile time —
        // the table must own the bytes it resolves after the source buffer is dropped — borrowing
        // the source rejected: `PineProgram` outlives every source buffer and is shared (P1).
        self.bytes.extend_from_slice(s.as_bytes());
        let end = u32::try_from(self.bytes.len()).expect("SymTable: more than u32::MAX bytes");
        self.ends.push(end);
        self.insert_index(id, s.as_bytes());
        Sym(id)
    }

    /// Look `s` up without interning it.
    #[must_use]
    pub fn get(&self, s: &str) -> Option<Sym> {
        let mask = self.index.len() - 1;
        let mut b = fnv1a64(s.as_bytes()) as usize & mask;
        loop {
            let v = self.index[b];
            if v == 0 {
                return None;
            }
            let sym = Sym(v - 1);
            if self.resolve_bytes(sym) == s.as_bytes() {
                return Some(sym);
            }
            b = (b + 1) & mask;
        }
    }

    /// The bytes of `s`. No allocation, no copy, no validation.
    /// An out-of-range symbol resolves to the empty slice (and asserts in
    /// debug): a `Sym` from another program is a caller bug, not a crash.
    #[inline]
    #[must_use]
    pub fn resolve_bytes(&self, s: Sym) -> &[u8] {
        let i = s.0 as usize;
        debug_assert!(i < self.ends.len(), "Sym({i}) out of range for this table");
        if i >= self.ends.len() {
            return &[];
        }
        let lo = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        let hi = self.ends[i] as usize;
        &self.bytes[lo..hi]
    }

    /// The text of `s`. Only `&str` is ever interned, so the bytes are valid
    /// UTF-8 by construction; the check here is the safe spelling of that
    /// invariant (it cannot fail, and the hot path uses `resolve_bytes`).
    #[inline]
    #[must_use]
    pub fn resolve(&self, s: Sym) -> &str {
        match core::str::from_utf8(self.resolve_bytes(s)) {
            Ok(t) => t,
            Err(_) => {
                debug_assert!(false, "SymTable holds non-UTF-8 bytes");
                ""
            }
        }
    }

    /// `(offset, len)` of `s` inside [`SymTable::bytes`] — how a `CONST`
    /// `StrRef` names an interned literal.
    #[inline]
    #[must_use]
    pub fn span_of(&self, s: Sym) -> (u32, u32) {
        let i = s.0 as usize;
        debug_assert!(i < self.ends.len());
        if i >= self.ends.len() {
            return (0, 0);
        }
        let lo = if i == 0 { 0 } else { self.ends[i - 1] };
        (lo, self.ends[i] - lo)
    }

    /// The whole byte blob: the program's `CONST` string region.
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `len` bytes at `off` in the blob, or the empty slice when the range
    /// is outside it (a malformed `StrRef`; asserts in debug).
    #[inline]
    #[must_use]
    pub fn str_at(&self, off: u32, len: u32) -> &[u8] {
        let lo = off as usize;
        let hi = lo.saturating_add(len as usize);
        debug_assert!(hi <= self.bytes.len(), "CONST StrRef outside the SymTable blob");
        if hi > self.bytes.len() {
            return &[];
        }
        &self.bytes[lo..hi]
    }

    /// Number of symbols, including `Sym(0)`.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Never true: `Sym(0)` is always present.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    fn insert_index(&mut self, id: u32, bytes: &[u8]) {
        let mask = self.index.len() - 1;
        let mut b = fnv1a64(bytes) as usize & mask;
        while self.index[b] != 0 {
            b = (b + 1) & mask;
        }
        self.index[b] = id + 1;
    }

    fn grow(&mut self) {
        let cap = self.index.len() * 2;
        self.index = vec![0; cap];
        let n = self.ends.len();
        let mut i = 0usize;
        while i < n {
            let lo = if i == 0 { 0 } else { self.ends[i - 1] as usize };
            let hi = self.ends[i] as usize;
            let mask = cap - 1;
            let mut b = fnv1a64(&self.bytes[lo..hi]) as usize & mask;
            while self.index[b] != 0 {
                b = (b + 1) & mask;
            }
            self.index[b] = i as u32 + 1;
            i += 1;
        }
    }
}
