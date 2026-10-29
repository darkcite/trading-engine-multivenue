// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool. No workspace crate depends on `pine-corpus`
//! (`hygiene_no_crate_depends_on_pine_corpus`), so neither the engine loop nor any bar or tick path
//! can reach it; every copy in this crate is cold.
//! ALLOC-DOCTRINE: offline operator tool; allocates freely (CLAUDE.md "Offline paths MAY
//! allocate"). No clock (contract §1.1), no `unsafe`, no `dyn`.
//! Reads a corpus at run time from a directory outside every git work tree; embeds no corpus
//! material.
//!
//! # `pine-corpus` — PVM-9's offline half (impl-09 v4, amended by AMEND-harvest 2026-10-29)
//!
//! Option A (R-P33, OH-1): the corpus is curated **by hand** by the operator. There is no
//! crawler and no program here ever opens a socket (I37). Intake is the offline verb
//! [`intake::import`] over the operator's provenance ledger `manual.tsv` and the pasted source
//! files; everything after intake is impl-09's offline pipeline: `scan` (one source, one parse,
//! every fact), dedup under two canonicalisation profiles, the built-in histograms and the
//! pre-screen, the per-kind top-N hand-off to PVM-10, the library plan and `verify`.

#![forbid(unsafe_code)]
#![deny(missing_docs, unused_must_use)]

pub mod build;
pub mod canon;
pub mod codes;
pub mod dedup;
pub mod hist;
pub mod index;
pub mod inputs;
pub mod intake;
pub mod jsonl;
pub mod layout;
pub mod libs;
pub mod licence;
pub mod rank;
pub mod scan;
pub mod schema;
pub mod verify;

mod ast_view; // the only reader of the pine-lang AST (I24)
mod bind;
mod spec_view; // the only user of pine_spec (I24)

pub use canon::{render, Profile};

/// Every corpus row carries `"schema": 1` as its first key (I20).
pub const SCHEMA: u32 = 1;
/// Version of the canonicaliser (§4.9). It is written into every `dedup.jsonl` and `topn-*.jsonl`
/// row and is bumped by any change to `canon.rs` that can change a hash.
pub const CANON_VERSION: u32 = 1;

/// Why a command stopped.
#[derive(Debug)]
pub enum CorpusError {
    /// A path lies inside a git work tree (L-A, I17).
    InsideWorkTree {
        /// The offending path.
        path: std::path::PathBuf,
        /// The work tree's top directory, i.e. the ancestor holding `.git`.
        top: std::path::PathBuf,
    },
    /// A source file whose SHA-256 is not its file name (I8).
    ShaMismatch {
        /// The file.
        path: std::path::PathBuf,
    },
    /// A malformed row, a row whose `schema` is not 1, or a corrupt line that is not the last one.
    Row {
        /// The file.
        path: std::path::PathBuf,
        /// 1-based line number.
        line: u64,
        /// What is wrong.
        what: &'static str,
    },
    /// The Fork collapse ratio is outside the stop band (D9-g). Exit code 4.
    Canary {
        /// `1e6 · (1 − clusters / scripts)`.
        collapse_ppm: u32,
    },
    /// Intake refused the provenance ledger as a whole (a file-level fault, not a row refusal:
    /// a missing header, a missing intake directory). Row refusals are reported, never raised.
    Intake {
        /// What is wrong.
        what: String,
    },
    /// Filesystem.
    Io(std::io::Error),
}

impl From<std::io::Error> for CorpusError {
    fn from(e: std::io::Error) -> Self {
        CorpusError::Io(e)
    }
}

impl std::fmt::Display for CorpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CorpusError::InsideWorkTree { path, top } => write!(
                f,
                "{} lies inside the git work tree {} (L-A, I17)",
                path.display(),
                top.display()
            ),
            CorpusError::ShaMismatch { path } => {
                write!(
                    f,
                    "{}: bytes do not hash to the file name (I8)",
                    path.display()
                )
            }
            CorpusError::Row { path, line, what } => {
                write!(f, "{}:{line}: {what}", path.display())
            }
            CorpusError::Canary { collapse_ppm } => write!(
                f,
                "fork collapse {collapse_ppm} ppm is outside [200000, 750000] (D9-g canary)"
            ),
            CorpusError::Intake { what } => write!(f, "intake: {what}"),
            CorpusError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

/// Lowercase hex of a SHA-256.
#[must_use]
pub fn hex64(h: &[u8; 32]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in h {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

/// Parse 64 lowercase hex characters (I20: `hex64` is lowercase only).
#[must_use]
pub fn parse_hex64(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        let hi = nib(b[2 * i])?;
        let lo = nib(b[2 * i + 1])?;
        *o = (hi << 4) | lo;
    }
    Some(out)
}

fn nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// SHA-256 of `data` (CAR-09-1).
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    core_crypto::sha256(data)
}

/// Write `bytes` to `path` through `<path>.tmp` and a rename (every output of §3.4).
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
