// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The arm's instrument table: engine `SymbolId` ↔ the venue's symbol.
//!
//! Built at boot from discovery (the engine) or from one `--symbol`
//! (the verbs). An order names its instrument by `SymbolId`; the venue
//! wants the exact `/markets` id string (it is signed byte for byte,
//! D7), and a fill names the string back. Both lookups are linear over
//! at most [`HC_TABLE_MAX`] rows — a submit or a fill, never a tick.

use core_types::SymbolId;

/// Rows the table holds (the ingress's `HC_INSTRUMENTS_MAX`).
pub const HC_TABLE_MAX: usize = 1024;
/// Longest venue symbol (`HcSymbolTable`'s inline cap).
pub const HC_NAME_MAX: usize = 32;

#[derive(Copy, Clone)]
struct Row {
    name: [u8; HC_NAME_MAX],
    len: u8,
    sym: SymbolId,
}

const EMPTY: Row = Row {
    name: [0; HC_NAME_MAX],
    len: 0,
    sym: core_types::SYMBOL_ID_NONE,
};

/// Why a row was refused.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TableErr {
    /// The table is full.
    Full,
    /// Empty, too long, or outside `[A-Za-z0-9._-]`.
    BadName,
    /// The symbol or the name is already in the table.
    Duplicate,
}

/// The table (module doc). Boot-built; read-only after.
pub struct HcInstruments {
    rows: Box<[Row]>,
    n: usize,
}

impl Default for HcInstruments {
    fn default() -> Self {
        Self::new()
    }
}

impl HcInstruments {
    /// An empty table. Boot-only: allocates its rows once.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: vec![EMPTY; HC_TABLE_MAX].into_boxed_slice(),
            n: 0,
        }
    }

    /// Add `name` as `sym`. Boot-only.
    ///
    /// # Errors
    ///
    /// [`TableErr`].
    pub fn insert(&mut self, sym: SymbolId, name: &[u8]) -> Result<(), TableErr> {
        if name.len() > HC_NAME_MAX || !crate::render::plain_ascii(name) {
            return Err(TableErr::BadName);
        }
        if self.name_of(sym).is_some() || self.sym_of(name).is_some() {
            return Err(TableErr::Duplicate);
        }
        if self.n >= self.rows.len() {
            return Err(TableErr::Full);
        }
        let r = &mut self.rows[self.n];
        // COPY: ≤ 32 B venue symbol into its row, once per instrument
        // at boot — the table must own the names it signs.
        r.name[..name.len()].copy_from_slice(name);
        r.len = name.len() as u8;
        r.sym = sym;
        self.n += 1;
        Ok(())
    }

    /// Rows held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.n
    }

    /// Is the table empty?
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The venue symbol of `sym`.
    #[must_use]
    pub fn name_of(&self, sym: SymbolId) -> Option<&[u8]> {
        let mut i = 0usize;
        while i < self.n {
            let r = &self.rows[i];
            if r.sym == sym {
                return Some(&r.name[..r.len as usize]);
            }
            i += 1;
        }
        None
    }

    /// The `SymbolId` of the venue symbol `name`.
    #[must_use]
    pub fn sym_of(&self, name: &[u8]) -> Option<SymbolId> {
        let mut i = 0usize;
        while i < self.n {
            let r = &self.rows[i];
            if &r.name[..r.len as usize] == name {
                return Some(r.sym);
            }
            i += 1;
        }
        None
    }

    /// The row index and `SymbolId` of the venue symbol `name`.
    #[must_use]
    pub fn find(&self, name: &[u8]) -> Option<(usize, SymbolId)> {
        let mut i = 0usize;
        while i < self.n {
            let r = &self.rows[i];
            if &r.name[..r.len as usize] == name {
                return Some((i, r.sym));
            }
            i += 1;
        }
        None
    }

    /// The row index of `sym`.
    #[must_use]
    pub fn find_sym(&self, sym: SymbolId) -> Option<usize> {
        let mut i = 0usize;
        while i < self.n {
            if self.rows[i].sym == sym {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Row `i`'s `(sym, name)` (cold: recon walks, the verbs).
    #[must_use]
    pub fn get(&self, i: usize) -> Option<(SymbolId, &[u8])> {
        if i >= self.n {
            return None;
        }
        let r = &self.rows[i];
        Some((r.sym, &r.name[..r.len as usize]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_directions_resolve_and_bad_rows_refuse() {
        let mut t = HcInstruments::new();
        t.insert(513, b"BTC-20261002-100000-C").unwrap();
        t.insert(514, b"SP500-20261002-7730.5-P").unwrap();
        assert_eq!(t.name_of(514), Some(&b"SP500-20261002-7730.5-P"[..]));
        assert_eq!(t.sym_of(b"BTC-20261002-100000-C"), Some(513));
        assert_eq!(t.sym_of(b"BTC-20261002-100000-P"), None);
        assert_eq!(t.insert(515, b"BTC-20261002-100000-C"), Err(TableErr::Duplicate));
        assert_eq!(t.insert(513, b"ETH-1"), Err(TableErr::Duplicate));
        assert_eq!(t.insert(516, b"has space"), Err(TableErr::BadName));
        assert_eq!(t.insert(516, &[b'A'; 33]), Err(TableErr::BadName));
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(1), Some((514, &b"SP500-20261002-7730.5-P"[..])));
    }
}
