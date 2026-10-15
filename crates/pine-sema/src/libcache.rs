// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! `LibCache` — library sources, never library `Ast`s (impl-02 §3.3, §4.2.1). With `code.rs`, the
//! only module that formats a `String`.

use std::path::PathBuf;

use pine_ir::{Diag, Span};

use crate::code::{diag, ReasonCode};

/// SHA-256 of a byte string. A host passes the fn item `core_crypto::sha256`
/// (`pub fn sha256(data: &[u8]) -> [u8; SHA256_LEN]`, `SHA256_LEN = 32`), which coerces to this
/// pointer as is — no closure, no wrapper. pine-sema has no `core-crypto` edge (contract §2); the
/// host has one.
pub type Sha256Fn = fn(&[u8]) -> [u8; 32];

/// `user/lib/ver` of an `import` (impl-01 `Node::Import`; `ver` is its `u16`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LibKey {
    /// The publishing user.
    pub user: String,
    /// The library name.
    pub lib: String,
    /// The version, `1..=65535`.
    pub ver: u16,
}

/// One library source the cache has read.
#[derive(Debug, Clone)]
pub struct LibSource {
    /// Which library.
    pub key: LibKey,
    /// SHA-256 of the bytes as read (zero for an in-memory entry).
    pub sha256: [u8; 32],
    /// `lib.lock` pins this key (and the pin matched: a mismatch is `3037`, never an entry).
    pub pinned: bool,
    text: String,
}

impl LibSource {
    /// The source text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone)]
enum Backing {
    Dir {
        root: PathBuf,
        sha256: Sha256Fn,
        lock: Vec<(LibKey, [u8; 32])>,
    },
    Memory {
        entries: Vec<(LibKey, String)>,
    },
    Empty,
}

/// Library sources by key. Holds **text only**: every compilation parses the libraries it imports
/// into its own `SymTable` (impl-01 §3.1 — an `Ast` is valid only with the table it was parsed
/// into), so one cache serves any number of compilations. `Send + Sync`; `compile` takes it
/// `&mut`, so a host compiling on several threads gives each worker its own `clone()` (clones
/// share nothing).
#[derive(Debug, Clone)]
pub struct LibCache {
    backing: Backing,
    loaded: Vec<LibSource>,
}

impl Default for LibCache {
    fn default() -> Self {
        Self::empty()
    }
}

impl LibCache {
    /// A cache over `root`, the directory that holds `<user>/<lib>/<ver>.pine` and `lib.lock`.
    /// Reads `<root>/lib.lock` once when it exists: lines `"user/lib/ver" = "<64 lowercase hex>"`.
    /// `root` need not exist: every `import` then refuses `3034` naming the path it tried. `Err`
    /// only for a `lib.lock` that exists and is unreadable, not UTF-8 or malformed (`3037`, the
    /// message `"<root>/lib.lock line N: …"`). Sources are read lazily, on first import, and kept
    /// for the cache's life.
    pub fn open(root: PathBuf, sha256: Sha256Fn) -> Result<LibCache, Diag> {
        let path = root.join("lib.lock");
        let lock = match std::fs::read(&path) {
            Ok(bytes) => read_lock(&bytes).map_err(|(line, why)| {
                let msg = format!("{} line {line}: {why}", path.display());
                diag(ReasonCode::ImportShaMismatch, Span::default(), msg)
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                let msg = format!("{} line 0: cannot read: {e}", path.display());
                return Err(diag(ReasonCode::ImportShaMismatch, Span::default(), msg));
            }
        };
        Ok(LibCache {
            backing: Backing::Dir { root, sha256, lock },
            loaded: Vec::new(),
        })
    }

    /// A cache over fixed sources (tests, conformance): no files, no lock, no hashing.
    #[must_use]
    pub fn in_memory(entries: Vec<(LibKey, String)>) -> LibCache {
        LibCache {
            backing: Backing::Memory { entries },
            loaded: Vec::new(),
        }
    }

    /// No libraries: every `import` is `3034`.
    #[must_use]
    pub fn empty() -> LibCache {
        LibCache {
            backing: Backing::Empty,
            loaded: Vec::new(),
        }
    }

    /// The sources read so far, in first-use order (so a host can list unpinned libraries).
    #[must_use]
    pub fn loaded(&self) -> &[LibSource] {
        &self.loaded
    }

    /// The index in [`LibCache::loaded`] of `key`'s source, reading it on first use (impl-02
    /// §4.2.1). `at` is the `import` span every refusal names.
    pub(crate) fn source(&mut self, key: &LibKey, at: Span) -> Result<usize, Diag> {
        let mut i = 0;
        while i < self.loaded.len() {
            if self.loaded[i].key == *key {
                return Ok(i);
            }
            i += 1;
        }
        if !segment_is_safe(&key.user) || !segment_is_safe(&key.lib) {
            let msg = format!(
                "invalid library path segment in import {}/{}/{}",
                key.user, key.lib, key.ver
            );
            return Err(diag(ReasonCode::ImportMissing, at, msg));
        }
        let src = match &self.backing {
            Backing::Dir { root, sha256, lock } => {
                let path = root
                    .join(&key.user)
                    .join(&key.lib)
                    .join(format!("{}.pine", key.ver));
                let missing = |why: &str| {
                    let msg = format!("library {}: {why}", path.display());
                    diag(ReasonCode::ImportMissing, at, msg)
                };
                let bytes = std::fs::read(&path).map_err(|_| missing("no such file"))?;
                let text = String::from_utf8(bytes).map_err(|_| missing("not UTF-8"))?;
                let digest = sha256(text.as_bytes());
                let mut pinned = false;
                let mut k = 0;
                while k < lock.len() {
                    if lock[k].0 == *key {
                        if lock[k].1 != digest {
                            let msg = format!(
                                "library {}: sha256 {} differs from lib.lock pin {}",
                                path.display(),
                                hex(&digest),
                                hex(&lock[k].1)
                            );
                            return Err(diag(ReasonCode::ImportShaMismatch, at, msg));
                        }
                        pinned = true;
                        break;
                    }
                    k += 1;
                }
                // COPY: the key is owned by the cache for its life (one per library, load time).
                LibSource {
                    key: key.clone(),
                    sha256: digest,
                    pinned,
                    text,
                }
            }
            Backing::Memory { entries } => {
                let mut k = 0;
                let mut found = None;
                while k < entries.len() {
                    if entries[k].0 == *key {
                        found = Some(k);
                        break;
                    }
                    k += 1;
                }
                let Some(k) = found else {
                    let msg = format!(
                        "library {}/{}/{}: no in-memory entry",
                        key.user, key.lib, key.ver
                    );
                    return Err(diag(ReasonCode::ImportMissing, at, msg));
                };
                // COPY: the in-memory entry stays available to clones; the loaded copy is the
                // cache's own record (one per library, load time).
                LibSource {
                    key: key.clone(),
                    sha256: [0; 32],
                    pinned: false,
                    text: entries[k].1.clone(),
                }
            }
            Backing::Empty => {
                let msg = format!(
                    "library {}/{}/{}: the cache has no libraries",
                    key.user, key.lib, key.ver
                );
                return Err(diag(ReasonCode::ImportMissing, at, msg));
            }
        };
        self.loaded.push(src);
        Ok(self.loaded.len() - 1)
    }
}

/// A segment equal to `.` or `..`, or holding `\` or NUL, could name a file outside `root`.
fn segment_is_safe(s: &str) -> bool {
    s != "." && s != ".." && !s.bytes().any(|b| b == b'\\' || b == 0)
}

fn hex(d: &[u8; 32]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    let mut i = 0;
    while i < 32 {
        s.push(H[usize::from(d[i] >> 4)] as char);
        s.push(H[usize::from(d[i] & 15)] as char);
        i += 1;
    }
    s
}

/// `ws` of the §4.2.1 grammar: space, tab, and the CR of a CRLF line end.
fn is_ws(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\r'
}

/// The `lib.lock` line scanner of impl-02 §4.2.1, total over any bytes. `Err((line, why))` names
/// the 1-based line of the first defect.
pub(crate) fn read_lock(bytes: &[u8]) -> Result<Vec<(LibKey, [u8; 32])>, (usize, &'static str)> {
    let mut out: Vec<(LibKey, [u8; 32])> = Vec::new();
    let mut line_no = 0usize;
    for raw in bytes.split(|&b| b == b'\n') {
        line_no += 1;
        let line = core::str::from_utf8(raw).map_err(|_| (line_no, "not UTF-8"))?;
        let b = line.as_bytes();
        if b.iter().all(|&c| is_ws(c)) || b[0] == b'#' {
            continue;
        }
        let (key, digest) = lock_line(line).ok_or((
            line_no,
            "expected \"user/lib/ver\" = \"<64 lowercase hex>\"",
        ))?;
        if out.iter().any(|(k, _)| *k == key) {
            return Err((line_no, "duplicate key"));
        }
        out.push((key, digest));
    }
    Ok(out)
}

/// `key ws* "=" ws* value ws*`.
fn lock_line(line: &str) -> Option<(LibKey, [u8; 32])> {
    let b = line.as_bytes();
    if b.first() != Some(&b'"') {
        return None;
    }
    let close = 1 + b[1..].iter().position(|&c| c == b'"')?;
    let key = &line[1..close];
    let mut parts = key.split('/');
    let (user, lib, ver) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !is_seg(user) || !is_seg(lib) {
        return None;
    }
    if ver.is_empty() || ver.len() > 5 || !ver.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let v: u32 = ver.parse().ok()?;
    if !(1..=65535).contains(&v) {
        return None;
    }
    let mut i = close + 1;
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    if b.get(i) != Some(&b'=') {
        return None;
    }
    i += 1;
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    if b.get(i) != Some(&b'"') || b.len() < i + 66 || b[i + 65] != b'"' {
        return None;
    }
    let mut digest = [0u8; 32];
    let mut k = 0;
    while k < 32 {
        digest[k] = (hex_val(b[i + 1 + 2 * k])? << 4) | hex_val(b[i + 2 + 2 * k])?;
        k += 1;
    }
    if !b[i + 66..].iter().all(|&c| is_ws(c)) {
        return None;
    }
    // COPY: the lock owns its keys (one per pinned library, read once at open).
    let key = LibKey {
        user: user.to_owned(),
        lib: lib.to_owned(),
        ver: v as u16,
    };
    Some((key, digest))
}

/// `seg := 1*(any byte except '"', '/', whitespace, NUL)`.
fn is_seg(s: &str) -> bool {
    !s.is_empty()
        && !s
            .bytes()
            .any(|c| c == b'"' || c == b'/' || c == 0 || c.is_ascii_whitespace())
}

/// Lowercase hex only (`0-9 | a-f`).
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}
