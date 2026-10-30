// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §3.6 / §4.13: the corpus layout and the work-tree refusal (L-A, I17). Under the
//! manual intake (AMEND-harvest) the layout is `index.jsonl`, `libs.jsonl`, `src/`, `lib/`,
//! `lib/lib.lock` and `out/`; the raw store and the crawl state are gone.

use std::path::{Path, PathBuf};

/// A corpus root that has been checked to lie outside every git work tree (I17).
#[derive(Debug, Clone)]
pub struct Corpus {
    root: PathBuf,
}

impl Corpus {
    /// Canonicalise `root`, then refuse it if it or any ancestor holds a `.git` entry.
    pub fn open(root: &Path) -> Result<Self, crate::CorpusError> {
        let root = std::fs::canonicalize(root)?;
        refuse_work_tree(&root)?;
        Ok(Corpus { root })
    }
    /// `<root>/src/<aa>/<hex>.pine`, where `<aa>` is the first two hex characters.
    #[must_use]
    pub fn src_path(&self, sha256: &[u8; 32]) -> PathBuf {
        let h = crate::hex64(sha256);
        self.root
            .join("src")
            .join(&h[..2])
            .join(format!("{h}.pine"))
    }
    /// `<root>/lib/<user>/<lib>/<ver>.pine`; `None` for a `Plan::bad_segments` key, so no path
    /// ever leaves `lib/`.
    #[must_use]
    pub fn lib_path(&self, key: &crate::scan::LibKey) -> Option<PathBuf> {
        if !crate::libs::segment_ok(&key.user) || !crate::libs::segment_ok(&key.lib) {
            return None;
        }
        Some(
            self.root
                .join("lib")
                .join(&key.user)
                .join(&key.lib)
                .join(format!("{}.pine", key.ver)),
        )
    }
    /// `<root>/out/`, created on first use. Outputs never go anywhere else.
    #[must_use]
    pub fn out_dir(&self) -> PathBuf {
        let d = self.root.join("out");
        let _ = std::fs::create_dir_all(&d);
        d
    }
    /// The root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// `<root>/index.jsonl`.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.jsonl")
    }
    /// `<root>/libs.jsonl`.
    #[must_use]
    pub fn libs_path(&self) -> PathBuf {
        self.root.join("libs.jsonl")
    }
    /// `<root>/lib/lib.lock`.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("lib").join("lib.lock")
    }
}

/// The nearest ancestor of `path` (inclusive) that holds a `.git` entry, if any.
#[must_use]
pub fn work_tree_top(path: &Path) -> Option<PathBuf> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p.join(".git").symlink_metadata().is_ok() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// `Err(InsideWorkTree)` when `path` (canonicalised by the caller) lies inside a git work tree.
pub fn refuse_work_tree(path: &Path) -> Result<(), crate::CorpusError> {
    match work_tree_top(path) {
        Some(top) => Err(crate::CorpusError::InsideWorkTree {
            path: path.to_path_buf(),
            top,
        }),
        None => Ok(()),
    }
}
