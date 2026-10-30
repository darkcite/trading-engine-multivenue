// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! The `index.jsonl` and `libs.jsonl` readers (last record wins, truncated-tail rule, I7) and
//! `corpus_sha256` (§3.5.10).

use std::collections::BTreeMap;
use std::path::Path;

use crate::schema::{IndexRow, LibsRow};
use crate::CorpusError;

/// The effective records of `index.jsonl`, ascending by slug.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Index {
    /// One `schema::IndexRow` (§3.5.2) per slug.
    pub records: Vec<IndexRow>,
    /// The largest `rec` read (0 when empty): the next append uses `max_rec + 1`.
    pub max_rec: u64,
}

fn read_file(path: &Path) -> Result<Vec<u8>, CorpusError> {
    match std::fs::read(path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(CorpusError::Io(e)),
    }
}

/// Read `index.jsonl` into its effective records: the last `rec` wins per slug, and a truncated
/// final line is dropped (I7). A missing file is an empty index.
pub fn load(path: &Path) -> Result<Index, CorpusError> {
    let bytes = read_file(path)?;
    load_bytes(&bytes, path)
}

/// [`load`] over bytes already read (`path` names them in errors).
pub fn load_bytes(bytes: &[u8], path: &Path) -> Result<Index, CorpusError> {
    let rows = crate::jsonl::read_lines(bytes).map_err(|(line, what)| CorpusError::Row {
        path: path.to_path_buf(),
        line,
        what,
    })?;
    let mut by_slug: BTreeMap<String, IndexRow> = BTreeMap::new();
    let mut max_rec = 0u64;
    let all: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    for (line_no, _) in rows {
        // Re-read the same line through the typed reader (the flat row already parsed).
        let l = all.get(line_no as usize - 1).copied().unwrap_or(&[]);
        let row = IndexRow::read(l).map_err(|what| CorpusError::Row {
            path: path.to_path_buf(),
            line: line_no,
            what,
        })?;
        max_rec = max_rec.max(row.rec);
        match by_slug.get(&row.core.slug) {
            Some(prev) if prev.rec >= row.rec => {}
            _ => {
                by_slug.insert(row.core.slug.clone(), row);
            }
        }
    }
    Ok(Index {
        records: by_slug.into_values().collect(),
        max_rec,
    })
}

/// The effective libraries of `libs.jsonl`, ascending by `(user, lib, ver)`, and the largest
/// `rec` read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Libs {
    /// One `schema::LibsRow` per key.
    pub records: Vec<LibsRow>,
    /// The largest `rec` read.
    pub max_rec: u64,
}

/// Read `libs.jsonl` the same way, one `schema::LibsRow` (§3.5.7) per `(user, lib, ver)`.
pub fn load_libs(path: &Path) -> Result<Vec<LibsRow>, CorpusError> {
    Ok(load_libs_full(path)?.records)
}

/// [`load_libs`] plus the largest `rec`.
pub fn load_libs_full(path: &Path) -> Result<Libs, CorpusError> {
    let bytes = read_file(path)?;
    let rows = crate::jsonl::read_lines(&bytes).map_err(|(line, what)| CorpusError::Row {
        path: path.to_path_buf(),
        line,
        what,
    })?;
    let mut by_key: BTreeMap<(String, String, u32), LibsRow> = BTreeMap::new();
    let mut max_rec = 0u64;
    let all: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    for (line_no, _) in rows {
        let l = all.get(line_no as usize - 1).copied().unwrap_or(&[]);
        let row = LibsRow::read(l).map_err(|what| CorpusError::Row {
            path: path.to_path_buf(),
            line: line_no,
            what,
        })?;
        max_rec = max_rec.max(row.rec);
        let key = (row.user.clone(), row.lib.clone(), row.ver);
        match by_key.get(&key) {
            Some(prev) if prev.rec >= row.rec => {}
            _ => {
                by_key.insert(key, row);
            }
        }
    }
    Ok(Libs {
        records: by_key.into_values().collect(),
        max_rec,
    })
}

/// SHA-256 over, for every effective record with `status == "parsed"`, in ascending byte order of
/// `slug`: `slug` (UTF-8) ‖ 0x00 ‖ `sha256` (64 lowercase hex, ASCII) ‖ 0x0A.
/// It is invariant under compaction and under the order of `index.jsonl`'s lines.
#[must_use]
pub fn corpus_sha256(index: &Index) -> [u8; 32] {
    let mut parsed: Vec<&IndexRow> = index
        .records
        .iter()
        .filter(|r| r.core.status == "parsed" && r.core.sha256.is_some())
        .collect();
    parsed.sort_by(|a, b| a.core.slug.as_bytes().cmp(b.core.slug.as_bytes()));
    let mut h = core_crypto::Sha256::new();
    for r in parsed {
        h.update(r.core.slug.as_bytes());
        h.update(&[0]);
        let hx = crate::hex64(r.core.sha256.as_ref().unwrap_or(&[0; 32]));
        h.update(hx.as_bytes());
        h.update(b"\n");
    }
    h.finalize()
}
