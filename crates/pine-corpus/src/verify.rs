// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! `pine-corpus verify`: I8 (source names; records → source files; the `raw_sha256` check only
//! when it is non-null — AMEND-harvest), I12 (`lib.lock`), I28 (conservation).

/// What `verify` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Sources checked.
    pub sources: u32,
    /// `parsed` records whose source or raw file is missing (slug, which file).
    pub missing: Vec<(String, &'static str)>,
    /// `lib.lock` keys that failed recomputation.
    pub lock_failures: Vec<String>,
    /// `pending`, `unavailable_src`, `parse_failed` and the Σ of Fork cluster sizes; they sum to
    /// the record count (§4.11.4).
    pub conservation: [u32; 4],
    /// The effective record count the conservation terms must sum to.
    pub records: u32,
}

impl VerifyReport {
    /// No finding at all.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.missing.is_empty()
            && self.lock_failures.is_empty()
            && self.conservation.iter().sum::<u32>() == self.records
    }
}

/// I8 (source names; records → source and raw files), I12 (`lib.lock`), I28 (conservation).
/// A source whose bytes do not hash to its name is `Err(CorpusError::ShaMismatch)`; every other
/// finding is listed in the report.
pub fn run(corpus: &crate::layout::Corpus) -> Result<VerifyReport, crate::CorpusError> {
    let mut sources = 0u32;
    let src = corpus.root().join("src");
    if let Ok(dirs) = std::fs::read_dir(&src) {
        let mut files = Vec::new();
        for d in dirs {
            let d = d?;
            if !d.file_type()?.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(d.path())? {
                files.push(f?.path());
            }
        }
        files.sort();
        for p in files {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Some(stem) = name.strip_suffix(".pine") else {
                continue;
            };
            let b = std::fs::read(&p)?;
            if crate::parse_hex64(stem) != Some(crate::sha256(&b)) {
                return Err(crate::CorpusError::ShaMismatch { path: p });
            }
            sources += 1;
        }
    }
    let index = crate::index::load(&corpus.index_path())?;
    let mut missing = Vec::new();
    for r in &index.records {
        if r.core.status != "parsed" {
            continue;
        }
        match r.core.sha256 {
            Some(h) if corpus.src_path(&h).exists() => {}
            _ => missing.push((r.core.slug.clone(), "src")),
        }
        if let Some(h) = r.core.raw_sha256 {
            let hx = crate::hex64(&h);
            let raw = corpus
                .root()
                .join("raw")
                .join(&hx[..2])
                .join(format!("{hx}.html.zst"));
            if !raw.exists() {
                missing.push((r.core.slug.clone(), "raw"));
            }
        }
    }
    let lock_failures = match crate::libs::verify_lock(corpus) {
        Ok(_) => Vec::new(),
        Err(v) => v,
    };
    let a = crate::build::analyse(corpus, 0b1111, &crate::rank::Weights::default(), None)?;
    Ok(VerifyReport {
        sources,
        missing,
        lock_failures,
        conservation: crate::hist::conservation(&a),
        records: a.scripts.len() as u32,
    })
}
