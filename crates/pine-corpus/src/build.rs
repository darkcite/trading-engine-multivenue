// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! The body of `build`, `dedup`, `hist` and `rank` (impl-09 §3.3): scan every distinct source and
//! every cached library once, resolve the library graph, apply the pre-screen and the licence
//! gate, cluster, then write what the options ask for. AMEND-harvest D9-g: an out-of-band
//! collapse is a **report, not a stop**, when every record was acquired by hand or the corpus
//! holds fewer than 200 parsed scripts.

use std::collections::{BTreeMap, BTreeSet};

use pine_ir::DiagCode;

use crate::dedup::{Clustering, Member};
use crate::index::Index;
use crate::libs::Reach;
use crate::scan::{LibKey, Scan};
use crate::schema::{DedupRow, IndexRow, TopnRow};
use crate::{codes, CorpusError, Profile};

/// What one run computes and writes. `dedup.jsonl` is always written, because `hist` and `rank`
/// need the clustering.
#[derive(Debug, Clone)]
pub struct BuildOpts {
    /// `CompileLimits::active_tiers` encoding (contract §3.8).
    pub tiers: u8,
    /// Rows in `topn-strategies.jsonl` (`u32::MAX` = all, AMEND-harvest `--n all`).
    pub n_strategies: u32,
    /// Rows in `topn-indicators.jsonl` (`u32::MAX` = all).
    pub n_indicators: u32,
    /// Ranking weights.
    pub weights: crate::rank::Weights,
    /// `None`: the largest `retrieved_ms` of the parsed records (§4.12).
    pub asof_ms: Option<i64>,
    /// Write `builtins.jsonl` and `buckets.jsonl`.
    pub write_hist: bool,
    /// Write the two top-N files.
    pub write_rank: bool,
}

/// What a run did: the CLI prints it, and the completion note quotes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSummary {
    /// Effective `index.jsonl` records.
    pub scripts: u32,
    /// Records whose source parsed.
    pub parsed_ok: u32,
    /// Fork collapse, ppm.
    pub collapse_ppm: u32,
    /// Fork representatives with no parse code and no 7101–7103 under `tiers`.
    pub prescreen_clean: u32,
    /// For each exclusion reason, the scripts whose *first* failing eligibility test it was
    /// (§4.12), ascending by code.
    pub excluded: Vec<(DiagCode, u32)>,
    /// The ranking date used.
    pub asof_ms: i64,
    /// §3.5.10.
    pub corpus_sha256: [u8; 32],
    /// The collapse is outside `[200 000, 750 000]` but under the manual intake or N < 200:
    /// reported, not a stop (AMEND-harvest D9-g).
    pub canary_reported: bool,
}

/// One effective record with every fact the outputs need.
#[derive(Debug, Clone)]
pub(crate) struct Script {
    pub(crate) rec: IndexRow,
    /// Index into `Analysis::scans`.
    pub(crate) scan: Option<usize>,
    pub(crate) parse_ok: bool,
    /// The unavailable entries of the own source and of every reached library (`U`, §4.11.2).
    pub(crate) unavail: BTreeSet<&'static str>,
    pub(crate) reach: Reach,
    /// 7105 details (`<class>`, `lib:<class>`).
    pub(crate) licence: BTreeSet<String>,
    /// §4.12 tests 1–5.
    pub(crate) eligible: bool,
    pub(crate) score: i64,
    pub(crate) exact: Option<[u8; 32]>,
    pub(crate) fork: Option<[u8; 32]>,
    pub(crate) uses: u64,
}

/// The shared analysis of one corpus under one tier mask.
#[derive(Debug)]
pub(crate) struct Analysis {
    pub(crate) scripts: Vec<Script>,
    pub(crate) scans: Vec<Scan>,
    /// Cached libraries: key → own scan index.
    pub(crate) lib_scans: BTreeMap<LibKey, usize>,
    pub(crate) fork: Clustering,
    pub(crate) exact: Clustering,
    /// Indices into `scripts` of the clustered (parse-ok) members, in member order.
    pub(crate) members: Vec<usize>,
    pub(crate) corpus_sha256: [u8; 32],
    pub(crate) spec_sha256: [u8; 32],
    pub(crate) tiers: u8,
    pub(crate) asof_ms: i64,
    pub(crate) all_manual: bool,
}

impl Analysis {
    pub(crate) fn scan_of(&self, s: &Script) -> Option<&Scan> {
        s.scan.map(|i| &self.scans[i])
    }
    /// The Fork cluster of `members[m]`.
    pub(crate) fn fork_rep_of(&self) -> BTreeMap<usize, usize> {
        let mut out = BTreeMap::new();
        for c in &self.fork.clusters {
            let rep = self.members[c.members[0] as usize];
            for &m in &c.members {
                out.insert(self.members[m as usize], rep);
            }
        }
        out
    }
    /// The Fork representatives (script indices), cluster order.
    pub(crate) fn reps(&self) -> Vec<usize> {
        self.fork
            .clusters
            .iter()
            .map(|c| self.members[c.members[0] as usize])
            .collect()
    }
}

/// Whether a pine-spec entry is unavailable under `tiers` (tier bit off, or declined).
pub(crate) fn unavailable(full: &str, tiers: u8) -> bool {
    crate::spec_view::lookup(full).is_some_and(|e| e.tier_bit() & tiers == 0 || e.declined())
}

fn own_class(r: &IndexRow) -> String {
    match (&r.licence_class, &r.core.licence_override) {
        (Some(c), _) => c.clone(),
        (None, None) => "mpl2".to_string(),
        (None, Some(_)) => "unknown".to_string(),
    }
}

/// Scan, resolve and cluster the corpus. A source whose bytes do not hash to its name is
/// `Err(ShaMismatch)` (I8).
pub(crate) fn analyse(
    corpus: &crate::layout::Corpus,
    tiers: u8,
    weights: &crate::rank::Weights,
    asof: Option<i64>,
) -> Result<Analysis, CorpusError> {
    let index: Index = crate::index::load(&corpus.index_path())?;
    let corpus_sha256 = crate::index::corpus_sha256(&index);
    let mut scans: Vec<Scan> = Vec::new();
    let mut by_sha: BTreeMap<[u8; 32], usize> = BTreeMap::new();
    let mut scripts: Vec<Script> = Vec::new();
    for r in &index.records {
        let mut scan = None;
        if r.core.status == "parsed" {
            if let Some(sha) = r.core.sha256 {
                let i = match by_sha.get(&sha) {
                    Some(&i) => i,
                    None => {
                        let p = corpus.src_path(&sha);
                        match std::fs::read(&p) {
                            Ok(b) => {
                                if crate::sha256(&b) != sha {
                                    return Err(CorpusError::ShaMismatch { path: p });
                                }
                                scans.push(crate::scan::scan(&String::from_utf8_lossy(&b)));
                                by_sha.insert(sha, scans.len() - 1);
                                scans.len() - 1
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => usize::MAX,
                            Err(e) => return Err(CorpusError::Io(e)),
                        }
                    }
                };
                if i != usize::MAX {
                    scan = Some(i);
                }
            }
        }
        let parse_ok = scan.is_some_and(|i| scans[i].parse_codes.is_empty());
        scripts.push(Script {
            rec: r.clone(),
            scan,
            parse_ok,
            unavail: BTreeSet::new(),
            reach: Reach::default(),
            licence: BTreeSet::new(),
            eligible: false,
            score: 0,
            exact: None,
            fork: None,
            uses: r.core.uses.unwrap_or(0),
        });
    }
    // Libraries.
    let mut lib_scans = BTreeMap::new();
    let mut cached = BTreeMap::new();
    for k in crate::libs::cached_keys(corpus)? {
        let Some(p) = corpus.lib_path(&k) else {
            continue;
        };
        let b = std::fs::read(p)?;
        let s = crate::scan::scan(&String::from_utf8_lossy(&b));
        cached.insert(k.clone(), s.imports.clone());
        scans.push(s);
        lib_scans.insert(k, scans.len() - 1);
    }
    let lib_rows = crate::index::load_libs(&corpus.libs_path())?;
    let lib_class: BTreeMap<LibKey, String> = lib_rows
        .iter()
        .filter(|r| r.outcome == "ok")
        .map(|r| {
            (
                LibKey {
                    user: r.user.clone(),
                    lib: r.lib.clone(),
                    ver: r.ver,
                },
                r.licence_class
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            )
        })
        .collect();
    let graph = crate::libs::Graph {
        mains: scripts
            .iter()
            .filter(|s| s.parse_ok)
            .map(|s| {
                (
                    s.rec.core.slug.clone(),
                    scans[s.scan.unwrap_or(0)].imports.clone(),
                )
            })
            .collect(),
        cached,
        permanent: crate::libs::permanent_of(&lib_rows),
    };
    let (_, mut reach) = crate::libs::walk(&graph);
    let asof_ms = asof.unwrap_or_else(|| {
        scripts
            .iter()
            .filter(|s| s.rec.core.status == "parsed")
            .filter_map(|s| s.rec.core.retrieved_ms)
            .max()
            .unwrap_or(0)
    });
    let all_manual = scripts
        .iter()
        .all(|s| s.rec.acq.as_deref() == Some("manual"));
    for s in &mut scripts {
        let class = own_class(&s.rec);
        if !crate::licence::admitted(&class) {
            s.licence.insert(class);
        }
        if !s.parse_ok {
            continue;
        }
        let sc = &scans[s.scan.unwrap_or(0)];
        s.reach = reach.remove(&s.rec.core.slug).unwrap_or_default();
        for u in &sc.uses {
            if unavailable(u.full, tiers) {
                s.unavail.insert(u.full);
            }
        }
        for k in &s.reach.libs {
            if let Some(&li) = lib_scans.get(k) {
                for u in &scans[li].uses {
                    if unavailable(u.full, tiers) {
                        s.unavail.insert(u.full);
                    }
                }
            }
            let c = lib_class
                .get(k)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            if !crate::licence::admitted(&c) {
                s.licence.insert(format!("lib:{c}"));
            }
        }
        s.exact = sc.h_exact;
        s.fork = sc.h_fork;
        let kind_ok = matches!(
            sc.kind,
            Some(pine_ir::ScriptKind::Strategy | pine_ir::ScriptKind::Indicator)
        );
        s.eligible = kind_ok
            && sc.dialect.is_some()
            && sc.unknown.is_empty()
            && s.reach.unresolved.is_empty()
            && s.unavail.is_empty()
            && s.licence.is_empty();
    }
    // Fork sizes first (membership does not depend on scores), then scores, then the order.
    let members: Vec<usize> = (0..scripts.len())
        .filter(|&i| scripts[i].parse_ok)
        .collect();
    let mut fork_size: BTreeMap<[u8; 32], u32> = BTreeMap::new();
    for &i in &members {
        if let Some(h) = scripts[i].fork {
            *fork_size.entry(h).or_insert(0) += 1;
        }
    }
    for &i in &members {
        let s = &mut scripts[i];
        let aliases = s.fork.map_or(1, |h| fork_size[&h]) - 1;
        s.score = crate::rank::score_1e6(
            s.rec.core.uses,
            s.rec.core.boosts,
            s.rec.core.likes,
            aliases,
            s.rec.core.updated_ms.or(s.rec.core.published_ms),
            asof_ms,
            weights,
        );
    }
    let mk = |profile: Profile| -> Clustering {
        let ms: Vec<Member<'_>> = members
            .iter()
            .map(|&i| {
                let s = &scripts[i];
                Member {
                    slug: &s.rec.core.slug,
                    hash: match profile {
                        Profile::Exact => s.exact.unwrap_or([0; 32]),
                        Profile::Fork => s.fork.unwrap_or([0; 32]),
                    },
                    eligible: s.eligible,
                    score_1e6: s.score,
                    uses: s.uses,
                    published_ms: s.rec.core.published_ms,
                }
            })
            .collect();
        crate::dedup::cluster(&ms, profile)
    };
    let fork = mk(Profile::Fork);
    let exact = mk(Profile::Exact);
    Ok(Analysis {
        scripts,
        scans,
        lib_scans,
        fork,
        exact,
        members,
        corpus_sha256,
        spec_sha256: crate::spec_view::spec_sha256(),
        tiers,
        asof_ms,
        all_manual,
    })
}

fn n_authors(a: &Analysis, idx: &[u32]) -> u32 {
    let set: BTreeSet<&str> = idx
        .iter()
        .filter_map(|&m| a.scripts[a.members[m as usize]].rec.core.author.as_deref())
        .collect();
    set.len() as u32
}

fn kind_str(k: Option<pine_ir::ScriptKind>) -> Option<String> {
    k.map(|k| {
        match k {
            pine_ir::ScriptKind::Strategy => "strategy",
            pine_ir::ScriptKind::Indicator => "indicator",
            pine_ir::ScriptKind::Library => "library",
        }
        .to_string()
    })
}

/// The `dedup.jsonl` bytes: `exact` rows first, then `fork`; each by size desc, then hash asc.
pub(crate) fn dedup_bytes(a: &Analysis) -> Vec<u8> {
    let mut out = Vec::new();
    for (c, profile) in [(&a.exact, "exact"), (&a.fork, "fork")] {
        for cl in &c.clusters {
            let rep = &a.scripts[a.members[cl.members[0] as usize]];
            let sc = a.scan_of(rep);
            DedupRow {
                profile: profile.to_string(),
                hash: cl.hash,
                fork: if profile == "exact" { rep.fork } else { None },
                size: cl.members.len() as u32,
                n_authors: n_authors(a, &cl.members),
                rep: rep.rec.core.slug.clone(),
                rep_sha256: rep.rec.core.sha256.unwrap_or([0; 32]),
                kind: kind_str(sc.and_then(|s| s.kind)),
                dialect: sc.and_then(|s| s.dialect).map_or(0, |d| d as u8),
                eligible: rep.eligible,
                aliases: cl.members[1..]
                    .iter()
                    .map(|&m| a.scripts[a.members[m as usize]].rec.core.slug.clone())
                    .collect(),
                canon: crate::CANON_VERSION,
                corpus_sha256: a.corpus_sha256,
                spec_sha256: a.spec_sha256,
                tiers: a.tiers,
            }
            .write(&mut out);
        }
    }
    out
}

/// The first failing eligibility code of a script (§4.12 tests 1, 2, 4, 5; test 3 — the kind —
/// and test 6 — the representative — carry no code).
pub(crate) fn first_failure(a: &Analysis, s: &Script) -> Option<DiagCode> {
    if s.rec.core.status != "parsed" || s.scan.is_none() {
        return Some(codes::SOURCE_UNAVAILABLE);
    }
    let sc = a.scan_of(s)?;
    if let Some(&c) = sc.parse_codes.first() {
        return Some(c);
    }
    if !matches!(
        sc.kind,
        Some(pine_ir::ScriptKind::Strategy | pine_ir::ScriptKind::Indicator)
    ) {
        return None;
    }
    if !sc.unknown.is_empty() {
        return Some(codes::BUILTIN_UNKNOWN);
    }
    if !s.reach.unresolved.is_empty() {
        return Some(codes::IMPORT_UNRESOLVED);
    }
    if !s.unavail.is_empty() {
        return Some(codes::BUILTIN_UNAVAILABLE);
    }
    if !s.licence.is_empty() {
        return Some(codes::LICENCE_RESTRICTED);
    }
    None
}

/// The two top-N files' bytes: `(strategies, indicators)`.
pub(crate) fn topn_bytes(a: &Analysis, opts: &BuildOpts) -> (Vec<u8>, Vec<u8>) {
    let rep_of = a.fork_rep_of();
    let mut files = (Vec::new(), Vec::new());
    for (kind, n, out) in [
        (
            pine_ir::ScriptKind::Strategy,
            opts.n_strategies,
            &mut files.0,
        ),
        (
            pine_ir::ScriptKind::Indicator,
            opts.n_indicators,
            &mut files.1,
        ),
    ] {
        let reps: Vec<usize> = a
            .reps()
            .into_iter()
            .filter(|&i| {
                let s = &a.scripts[i];
                s.eligible && a.scan_of(s).and_then(|x| x.kind) == Some(kind)
            })
            .collect();
        let cands: Vec<crate::rank::Candidate> = reps
            .iter()
            .map(|&i| {
                let s = &a.scripts[i];
                let size = rep_of.values().filter(|&&r| r == i).count() as u32;
                crate::rank::Candidate {
                    slug: s.rec.core.slug.clone(),
                    author: s.rec.core.author.clone(),
                    score_1e6: s.score,
                    uses: s.uses,
                    cluster_size: size,
                    published_ms: s.rec.core.published_ms,
                }
            })
            .collect();
        let pick = crate::rank::select(
            &cands,
            (n != u32::MAX).then_some(n),
            opts.weights.max_per_author,
        );
        for (rank, &ci) in pick.iter().enumerate() {
            let i = reps[ci];
            let s = &a.scripts[i];
            let Some(sc) = a.scan_of(s) else { continue };
            let cluster = a
                .fork
                .clusters
                .iter()
                .find(|c| a.members[c.members[0] as usize] == i);
            let (aliases, n_auth) = match cluster {
                Some(c) => (
                    c.members[1..]
                        .iter()
                        .map(|&m| a.scripts[a.members[m as usize]].rec.core.slug.clone())
                        .collect(),
                    n_authors(a, &c.members),
                ),
                None => (Vec::new(), 0),
            };
            let flags = |f: u16| sc.inputs.iter().filter(|x| x.desc.flags & f != 0).count() as u32;
            TopnRow {
                rank: rank as u32 + 1,
                score_1e6: s.score,
                core: s.rec.core.clone(),
                kind: kind_str(Some(kind)).unwrap_or_default(),
                dialect: sc.dialect.map_or(0, |d| d as u8),
                cluster: s.fork.unwrap_or([0; 32]),
                cluster_size: cands[ci].cluster_size,
                aliases,
                n_authors: n_auth,
                libs: s.reach.libs.iter().map(LibKey::text).collect(),
                n_inputs: sc.inputs.len() as u32,
                k_numeric: crate::inputs::k_numeric(&sc.inputs),
                k_dead: flags(pine_ir::input_flag::DEAD),
                k_options: flags(pine_ir::input_flag::HAS_OPTIONS),
                k_bounded: flags(pine_ir::input_flag::HAS_MIN | pine_ir::input_flag::HAS_MAX),
                plot_top: sc.plots.plot_top,
                plot_nested: sc.plots.plot_nested,
                hline: sc.plots.hline,
                canon: crate::CANON_VERSION,
                corpus_sha256: a.corpus_sha256,
                spec_sha256: a.spec_sha256,
                tiers: a.tiers,
                asof_ms: a.asof_ms,
                weights: opts.weights,
                acq: s.rec.acq.clone(),
                licence_class: s.rec.licence_class.clone(),
            }
            .write(out);
        }
    }
    files
}

/// Scan every distinct source and every cached library once, cluster them, then write what `opts`
/// asks for. On a canary (§4.9.7, amended) it returns `Err(CorpusError::Canary)` after writing
/// `dedup.jsonl` and nothing else.
pub fn build(
    corpus: &crate::layout::Corpus,
    opts: &BuildOpts,
) -> Result<BuildSummary, CorpusError> {
    let a = analyse(corpus, opts.tiers, &opts.weights, opts.asof_ms)?;
    let out = corpus.out_dir();
    crate::write_atomic(&out.join("dedup.jsonl"), &dedup_bytes(&a))?;
    let ppm = a.fork.collapse_ppm;
    let out_of_band = !(200_000..=750_000).contains(&ppm);
    let parsed_ok = a.members.len() as u32;
    let manual_or_small = a.all_manual || parsed_ok < 200;
    if out_of_band && !manual_or_small {
        return Err(CorpusError::Canary { collapse_ppm: ppm });
    }
    if opts.write_hist {
        let (b, k) = crate::hist::bytes(&a);
        crate::write_atomic(&out.join("builtins.jsonl"), &b)?;
        crate::write_atomic(&out.join("buckets.jsonl"), &k)?;
    }
    if opts.write_rank {
        let (s, i) = topn_bytes(&a, opts);
        crate::write_atomic(&out.join("topn-strategies.jsonl"), &s)?;
        crate::write_atomic(&out.join("topn-indicators.jsonl"), &i)?;
    }
    let mut excluded: BTreeMap<u16, u32> = BTreeMap::new();
    for s in &a.scripts {
        if let Some(c) = first_failure(&a, s) {
            *excluded.entry(c.0).or_insert(0) += 1;
        }
    }
    let prescreen_clean = a
        .reps()
        .into_iter()
        .filter(|&i| {
            let s = &a.scripts[i];
            a.scan_of(s).is_some_and(|sc| sc.unknown.is_empty())
                && s.reach.unresolved.is_empty()
                && s.unavail.is_empty()
        })
        .count() as u32;
    Ok(BuildSummary {
        scripts: a.scripts.len() as u32,
        parsed_ok,
        collapse_ppm: ppm,
        prescreen_clean,
        excluded: excluded
            .into_iter()
            .map(|(c, n)| (DiagCode(c), n))
            .collect(),
        asof_ms: a.asof_ms,
        corpus_sha256: a.corpus_sha256,
        canary_reported: out_of_band,
    })
}

/// `pine-corpus libs`: the plan, and `out/libs-want.jsonl` (the operator's copy list).
pub fn libs(corpus: &crate::layout::Corpus) -> Result<crate::libs::Plan, CorpusError> {
    let plan = crate::libs::plan(corpus)?;
    let mut out = Vec::new();
    for w in &plan.wants {
        crate::schema::WantRow {
            user: w.key.user.clone(),
            lib: w.key.lib.clone(),
            ver: w.key.ver,
            depth: w.depth,
            dependents: w.dependents,
        }
        .write(&mut out);
    }
    crate::write_atomic(&corpus.out_dir().join("libs-want.jsonl"), &out)?;
    Ok(plan)
}
