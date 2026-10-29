// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Shared test helpers: fixtures and throw-away corpora under the system temp dir (outside every
//! git work tree, I17). Every fixture is hand-written (`// OUR CODE — not harvested`).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use pine_corpus::layout::Corpus;
use pine_corpus::schema::{IndexCore, IndexRow};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// The crate's fixture directory.
pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A fixture's text.
pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(name)).expect(name)
}

/// A temp dir removed on drop.
pub struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("pine-corpus-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One script for a test corpus.
#[derive(Clone)]
pub struct S {
    pub slug: String,
    pub src: String,
    pub author: Option<String>,
    pub uses: Option<u64>,
    pub published_ms: Option<i64>,
    pub status: String,
    pub licence_class: String,
    pub licence_override: Option<String>,
    pub acq: String,
}

impl S {
    pub fn new(slug: &str, src: &str) -> Self {
        S {
            slug: slug.to_string(),
            src: src.to_string(),
            author: Some("alice".to_string()),
            uses: Some(10),
            published_ms: Some(1_600_000_000_000),
            status: "parsed".to_string(),
            licence_class: "mpl2".to_string(),
            licence_override: None,
            acq: "manual".to_string(),
        }
    }
    pub fn author(mut self, a: &str) -> Self {
        self.author = Some(a.to_string());
        self
    }
    pub fn uses(mut self, u: u64) -> Self {
        self.uses = Some(u);
        self
    }
    pub fn class(mut self, c: &str) -> Self {
        self.licence_class = c.to_string();
        self
    }
    pub fn acq(mut self, a: &str) -> Self {
        self.acq = a.to_string();
        self
    }
    pub fn status(mut self, s: &str) -> Self {
        self.status = s.to_string();
        self
    }
}

/// An index row for `s`, sourced from `sha`.
pub fn row(rec: u64, s: &S, sha: Option<[u8; 32]>) -> IndexRow {
    IndexRow {
        rec,
        core: IndexCore {
            slug: s.slug.clone(),
            url: format!("https://example.invalid/script/{}-x/", s.slug),
            title: s.slug.clone(),
            author: s.author.clone(),
            listed_type: "indicator".to_string(),
            open_source: true,
            published_ms: s.published_ms,
            updated_ms: None,
            likes: None,
            boosts: None,
            uses: s.uses,
            listed_ms: 1_760_000_000_000,
            status: s.status.clone(),
            http_status: None,
            raw_sha256: None,
            retrieved_ms: Some(1_760_000_000_000),
            sha256: sha,
            src_bytes: Some(s.src.len() as u32),
            version_src: Some(5),
            version_page: Some(5),
            licence_override: s.licence_override.clone(),
        },
        acq: Some(s.acq.clone()),
        licence_class: Some(s.licence_class.clone()),
    }
}

/// A corpus holding `scripts` (sources written, index rows appended in the given order) and
/// optional cached libraries `(user/lib/ver, src)`.
pub fn corpus(tag: &str, scripts: &[S], libs: &[(&str, &str)]) -> (Tmp, Corpus) {
    let t = Tmp::new(tag);
    let root = t.path().join("corpus");
    std::fs::create_dir_all(&root).unwrap();
    let c = Corpus::open(&root).unwrap();
    let mut idx = Vec::new();
    for (i, s) in scripts.iter().enumerate() {
        let sha = if s.status == "parsed" {
            let h = pine_corpus::sha256(s.src.as_bytes());
            let p = c.src_path(&h);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, &s.src).unwrap();
            Some(h)
        } else {
            None
        };
        row(i as u64 + 1, s, sha).write(&mut idx);
    }
    std::fs::write(c.index_path(), &idx).unwrap();
    let mut lrows = Vec::new();
    for (i, (k, src)) in libs.iter().enumerate() {
        let p = c.root().join("lib").join(format!("{k}.pine"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, src).unwrap();
        let mut parts = k.split('/');
        pine_corpus::schema::LibsRow {
            rec: i as u64 + 1,
            user: parts.next().unwrap().to_string(),
            lib: parts.next().unwrap().to_string(),
            ver: parts.next().unwrap().parse().unwrap(),
            outcome: "ok".to_string(),
            http_status: None,
            retrieved_ms: Some(1_760_000_000_000),
            sha256: Some(pine_corpus::sha256(src.as_bytes())),
            attempts: 0,
            deferred_days: 0,
            licence_class: Some("mpl2".to_string()),
        }
        .write(&mut lrows);
    }
    if !libs.is_empty() {
        std::fs::write(c.libs_path(), &lrows).unwrap();
        pine_corpus::intake::write_lock(&c).unwrap();
    }
    (t, c)
}

/// The default build options under `tiers`.
pub fn opts(tiers: u8) -> pine_corpus::build::BuildOpts {
    pine_corpus::build::BuildOpts {
        tiers,
        n_strategies: u32::MAX,
        n_indicators: u32::MAX,
        weights: pine_corpus::rank::Weights::default(),
        asof_ms: None,
        write_hist: true,
        write_rank: true,
    }
}

/// A file under `<corpus>/out/`.
pub fn out(c: &Corpus, name: &str) -> Vec<u8> {
    std::fs::read(c.root().join("out").join(name)).unwrap_or_default()
}

/// A tiny Pine indicator with a body.
pub fn ind(body: &str) -> String {
    format!("//@version=5\nindicator(\"t\")\n{body}\n")
}
