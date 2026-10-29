// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §4.10: the import graph, the want-list, cycles, the depth bound, case collisions and
//! `verify_lock`. Under the manual intake (AMEND-harvest) the want-list is printed as the
//! operator's copy list and `pine-corpus import-lib` writes what he copies; a want with no
//! `libs.jsonl` record yet is `pending` (7103 `pending`).

use std::collections::{BTreeMap, BTreeSet};

use crate::scan::LibKey;

/// impl-02's import depth bound (impl-02 v4 §3.4: `pine_sema::limits::MAX_IMPORT_DEPTH = 8`, root = 0),
/// measured from the main script. It is a cross-spec constant with no crate edge to check it;
/// `libs_max_import_depth_is_eight` pins the value, and a change in impl-02 is a change here.
pub const MAX_IMPORT_DEPTH: u32 = 8;

/// One library to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    /// Which library.
    pub key: LibKey,
    /// Shallowest depth at which it is imported (1 = imported by a main script).
    pub depth: u32,
    /// Main scripts that reach it.
    pub dependents: u32,
}

/// The state of the library fixpoint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Missing from the cache, depth ≤ `MAX_IMPORT_DEPTH`, and with no permanent outcome yet.
    pub wants: Vec<Want>,
    /// Beyond `MAX_IMPORT_DEPTH`: recorded, never wanted (I13).
    pub over_depth: Vec<Want>,
    /// Each cycle as its key path, `a/x/1 -> b/y/2 -> a/x/1`.
    pub cycles: Vec<Vec<LibKey>>,
    /// Two keys equal under ASCII case folding; neither is wanted.
    pub case_collisions: Vec<(LibKey, LibKey)>,
    /// Keys whose `user` or `lib` is `.` or `..`, or holds `\` or NUL: never wanted or given a
    /// path (7103 `bad_segment`).
    pub bad_segments: Vec<LibKey>,
    /// Imports that resolve to a cache file.
    pub resolved: u32,
}

/// What one main script reaches through its imports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reach {
    /// Transitive imports that resolve to a cache file, ascending.
    pub libs: BTreeSet<LibKey>,
    /// 7103 details for this script, ascending, unique.
    pub unresolved: BTreeSet<&'static str>,
}

/// A `user` or `lib` segment that can be a path component (impl-02 v4 §4.2.1 refuses the rest).
#[must_use]
pub fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('\\')
        && !s.contains('\0')
        && !s.contains('/')
}

fn key_ok(k: &LibKey) -> bool {
    segment_ok(&k.user) && segment_ok(&k.lib)
}

fn folded(k: &LibKey) -> (String, String, u32) {
    (
        k.user.to_ascii_lowercase(),
        k.lib.to_ascii_lowercase(),
        k.ver,
    )
}

/// The graph's inputs, independent of the filesystem.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    /// Main scripts: `(slug, direct imports in source order)`.
    pub mains: Vec<(String, Vec<LibKey>)>,
    /// Cached libraries and their own direct imports.
    pub cached: BTreeMap<LibKey, Vec<LibKey>>,
    /// Effective permanent outcomes from `libs.jsonl` (`lib_missing`, `lib_private`,
    /// `lib_version_missing`).
    pub permanent: BTreeMap<LibKey, &'static str>,
}

fn permanent_detail(o: &str) -> Option<&'static str> {
    match o {
        "lib_missing" => Some("lib_missing"),
        "lib_private" => Some("lib_private"),
        "lib_version_missing" => Some("lib_version_missing"),
        _ => None,
    }
}

/// Build the graph's permanent-outcome map from effective `libs.jsonl` rows.
#[must_use]
pub fn permanent_of(rows: &[crate::schema::LibsRow]) -> BTreeMap<LibKey, &'static str> {
    rows.iter()
        .filter_map(|r| {
            permanent_detail(&r.outcome).map(|d| {
                (
                    LibKey {
                        user: r.user.clone(),
                        lib: r.lib.clone(),
                        ver: r.ver,
                    },
                    d,
                )
            })
        })
        .collect()
}

/// Walk the graph: the plan, and per main script what it reaches.
#[must_use]
pub fn walk(g: &Graph) -> (Plan, BTreeMap<String, Reach>) {
    // Case collisions over every key the graph names.
    let mut all: BTreeSet<LibKey> = g.cached.keys().cloned().collect();
    for (_, ims) in &g.mains {
        all.extend(ims.iter().cloned());
    }
    for ims in g.cached.values() {
        all.extend(ims.iter().cloned());
    }
    let mut by_fold: BTreeMap<(String, String, u32), Vec<LibKey>> = BTreeMap::new();
    for k in &all {
        by_fold.entry(folded(k)).or_default().push(k.clone());
    }
    let mut collide: BTreeSet<LibKey> = BTreeSet::new();
    let mut case_collisions = Vec::new();
    for ks in by_fold.values() {
        if ks.len() > 1 {
            for w in ks.windows(2) {
                case_collisions.push((w[0].clone(), w[1].clone()));
            }
            collide.extend(ks.iter().cloned());
        }
    }

    let mut wants: BTreeMap<LibKey, (u32, BTreeSet<String>)> = BTreeMap::new();
    let mut over: BTreeMap<LibKey, (u32, BTreeSet<String>)> = BTreeMap::new();
    let mut cycles: BTreeSet<Vec<LibKey>> = BTreeSet::new();
    let mut bad: BTreeSet<LibKey> = BTreeSet::new();
    let mut resolved = 0u32;
    let mut reach: BTreeMap<String, Reach> = BTreeMap::new();

    for (slug, ims) in &g.mains {
        let mut r = Reach::default();
        // Explicit-stack DFS: (key, depth, child cursor). The path is the stack's keys.
        let mut stack: Vec<(LibKey, u32, usize)> = Vec::new();
        let mut done: BTreeSet<(LibKey, u32)> = BTreeSet::new();
        let visit = |k: &LibKey,
                     depth: u32,
                     path: &[(LibKey, u32, usize)],
                     r: &mut Reach,
                     wants: &mut BTreeMap<LibKey, (u32, BTreeSet<String>)>,
                     over: &mut BTreeMap<LibKey, (u32, BTreeSet<String>)>,
                     cycles: &mut BTreeSet<Vec<LibKey>>,
                     bad: &mut BTreeSet<LibKey>,
                     resolved: &mut u32|
         -> bool {
            if !key_ok(k) {
                bad.insert(k.clone());
                r.unresolved.insert("bad_segment");
                return false;
            }
            if let Some(pos) = path.iter().position(|(x, _, _)| x == k) {
                let mut c: Vec<LibKey> = path[pos..].iter().map(|(x, _, _)| x.clone()).collect();
                c.push(k.clone());
                cycles.insert(c);
                r.unresolved.insert("cycle");
                return false;
            }
            if depth > MAX_IMPORT_DEPTH {
                let e = over.entry(k.clone()).or_insert((depth, BTreeSet::new()));
                e.0 = e.0.min(depth);
                e.1.insert(slug.clone());
                r.unresolved.insert("over_depth");
                return false;
            }
            if collide.contains(k) {
                r.unresolved.insert("case_collision");
                return false;
            }
            if g.cached.contains_key(k) {
                *resolved += 1;
                r.libs.insert(k.clone());
                return true;
            }
            if let Some(d) = g.permanent.get(k) {
                r.unresolved.insert(d);
                return false;
            }
            let e = wants.entry(k.clone()).or_insert((depth, BTreeSet::new()));
            e.0 = e.0.min(depth);
            e.1.insert(slug.clone());
            r.unresolved.insert("pending");
            false
        };
        for k in ims {
            if visit(
                k,
                1,
                &stack,
                &mut r,
                &mut wants,
                &mut over,
                &mut cycles,
                &mut bad,
                &mut resolved,
            ) && done.insert((k.clone(), 1))
            {
                stack.push((k.clone(), 1, 0));
            }
            while let Some((key, depth, cur)) = stack.last().cloned() {
                let kids = g.cached.get(&key).cloned().unwrap_or_default();
                if cur >= kids.len() {
                    stack.pop();
                    continue;
                }
                if let Some(top) = stack.last_mut() {
                    top.2 += 1;
                }
                let child = &kids[cur];
                if visit(
                    child,
                    depth + 1,
                    &stack,
                    &mut r,
                    &mut wants,
                    &mut over,
                    &mut cycles,
                    &mut bad,
                    &mut resolved,
                ) && done.insert((child.clone(), depth + 1))
                {
                    stack.push((child.clone(), depth + 1, 0));
                }
            }
        }
        reach.insert(slug.clone(), r);
    }
    let to_want = |m: BTreeMap<LibKey, (u32, BTreeSet<String>)>| -> Vec<Want> {
        m.into_iter()
            .map(|(key, (depth, deps))| Want {
                key,
                depth,
                dependents: deps.len() as u32,
            })
            .collect()
    };
    let plan = Plan {
        wants: to_want(wants),
        over_depth: to_want(over),
        cycles: cycles.into_iter().collect(),
        case_collisions,
        bad_segments: bad.into_iter().collect(),
        resolved,
    };
    (plan, reach)
}

/// A cycle's path text, `a/x/1 -> b/y/2 -> a/x/1`.
#[must_use]
pub fn cycle_text(c: &[LibKey]) -> String {
    c.iter().map(LibKey::text).collect::<Vec<_>>().join(" -> ")
}

/// Every cached library file under `<root>/lib/`, as keys (`<user>/<lib>/<ver>.pine`).
pub fn cached_keys(corpus: &crate::layout::Corpus) -> Result<Vec<LibKey>, crate::CorpusError> {
    let mut out = Vec::new();
    let lib = corpus.root().join("lib");
    let Ok(users) = std::fs::read_dir(&lib) else {
        return Ok(out);
    };
    for u in users {
        let u = u?;
        if !u.file_type()?.is_dir() {
            continue;
        }
        let user = u.file_name().to_string_lossy().into_owned();
        for l in std::fs::read_dir(u.path())? {
            let l = l?;
            if !l.file_type()?.is_dir() {
                continue;
            }
            let libn = l.file_name().to_string_lossy().into_owned();
            for v in std::fs::read_dir(l.path())? {
                let v = v?;
                let name = v.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".pine") else {
                    continue;
                };
                if let Ok(ver) = stem.parse::<u32>() {
                    out.push(LibKey {
                        user: user.clone(),
                        lib: libn.clone(),
                        ver,
                    });
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Build the plan from every parsed main source and every cached library (§4.10).
pub fn plan(corpus: &crate::layout::Corpus) -> Result<Plan, crate::CorpusError> {
    let g = graph(corpus)?;
    Ok(walk(&g).0)
}

/// Read the graph's inputs from a corpus: each distinct main source is scanned once.
pub fn graph(corpus: &crate::layout::Corpus) -> Result<Graph, crate::CorpusError> {
    let index = crate::index::load(&corpus.index_path())?;
    let mut by_sha: BTreeMap<[u8; 32], Vec<LibKey>> = BTreeMap::new();
    let mut mains = Vec::new();
    for r in &index.records {
        if r.core.status != "parsed" {
            continue;
        }
        let Some(sha) = r.core.sha256 else { continue };
        if let std::collections::btree_map::Entry::Vacant(e) = by_sha.entry(sha) {
            let src = std::fs::read(corpus.src_path(&sha))?;
            let s = crate::scan::scan(&String::from_utf8_lossy(&src));
            e.insert(s.imports);
        }
        mains.push((r.core.slug.clone(), by_sha[&sha].clone()));
    }
    let mut cached = BTreeMap::new();
    for k in cached_keys(corpus)? {
        let Some(p) = corpus.lib_path(&k) else {
            continue;
        };
        let src = std::fs::read(p)?;
        let s = crate::scan::scan(&String::from_utf8_lossy(&src));
        cached.insert(k, s.imports);
    }
    let libs = crate::index::load_libs(&corpus.libs_path())?;
    Ok(Graph {
        mains,
        cached,
        permanent: permanent_of(&libs),
    })
}

/// Parse `lib.lock` (§3.5.9): `"<user>/<lib>/<ver>" = "<64 lowercase hex>"` lines. A line that
/// is not of that form is returned in the error list as itself.
#[must_use]
pub fn parse_lock(text: &str) -> (Vec<(String, [u8; 32])>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for line in text.lines() {
        let parsed = (|| {
            let (k, v) = line.split_once(" = ")?;
            let k = k.strip_prefix('"')?.strip_suffix('"')?;
            let v = v.strip_prefix('"')?.strip_suffix('"')?;
            Some((k.to_string(), crate::parse_hex64(v)?))
        })();
        match parsed {
            Some(e) => ok.push(e),
            None => bad.push(line.to_string()),
        }
    }
    (ok, bad)
}

/// Render `lib.lock`: keys in ascending byte order, nothing else.
#[must_use]
pub fn render_lock(entries: &BTreeMap<String, [u8; 32]>) -> String {
    let mut s = String::new();
    for (k, h) in entries {
        s.push('"');
        s.push_str(k);
        s.push_str("\" = \"");
        s.push_str(&crate::hex64(h));
        s.push_str("\"\n");
    }
    s
}

/// Recompute every `lib.lock` entry. `Ok(n)` is the number of entries verified; `Err` lists the
/// keys whose file is missing or whose SHA-256 differs (I12).
pub fn verify_lock(corpus: &crate::layout::Corpus) -> Result<u32, Vec<String>> {
    let text = match std::fs::read_to_string(corpus.lock_path()) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(vec![format!("lib.lock: {e}")]),
    };
    let (entries, mut bad) = parse_lock(&text);
    let mut n = 0u32;
    for (k, h) in entries {
        let mut parts = k.splitn(3, '/');
        let (Some(u), Some(l), Some(v)) = (parts.next(), parts.next(), parts.next()) else {
            bad.push(k);
            continue;
        };
        let Ok(ver) = v.parse::<u32>() else {
            bad.push(k);
            continue;
        };
        let key = LibKey {
            user: u.to_string(),
            lib: l.to_string(),
            ver,
        };
        let ok = corpus
            .lib_path(&key)
            .and_then(|p| std::fs::read(p).ok())
            .is_some_and(|b| crate::sha256(&b) == h);
        if ok {
            n += 1;
        } else {
            bad.push(k);
        }
    }
    if bad.is_empty() {
        Ok(n)
    } else {
        Err(bad)
    }
}
