// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §4.11: `builtins.jsonl` (with the frequency `rank` and `blocking_only`, the number a
//! tier-4 decision turns on) and `buckets.jsonl` (one row per `(DiagCode, detail)`), plus the
//! conservation identity (I28) and an independent second count of every `builtins.jsonl` column.

use std::collections::{BTreeMap, BTreeSet};

use pine_ir::{DiagCode, DiagRange};

use crate::build::Analysis;
use crate::schema::{BucketRow, BuiltinRow};
use crate::{codes, CorpusError};

/// `DiagCode::range()` lowercased.
#[must_use]
pub fn range_name(c: DiagCode) -> &'static str {
    match c.range() {
        DiagRange::Lexer => "lexer",
        DiagRange::Parser => "parser",
        DiagRange::Sema => "sema",
        DiagRange::Verifier => "verifier",
        DiagRange::Std => "std",
        DiagRange::Broker => "broker",
        DiagRange::Host => "host",
        DiagRange::Harness => "harness",
        DiagRange::Unassigned => "unassigned",
    }
}

#[derive(Default, Clone)]
struct Acc {
    scripts_raw: u32,
    scripts_dedup: u32,
    sites_call: u64,
    sites_value: u64,
    sites_type: u64,
    uses_raw: u64,
    in_strategies: u32,
    in_indicators: u32,
    in_libs: u32,
    v5: u32,
    v6: u32,
    blocking_only: u32,
    tops: Vec<(i64, String)>,
}

/// The rows of `builtins.jsonl`, in file order (first pass: representatives in cluster order).
fn builtin_rows(a: &Analysis) -> Vec<BuiltinRow> {
    let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
    // Raw universe: parse-ok slugs.
    for &i in &a.members {
        let s = &a.scripts[i];
        let Some(sc) = a.scan_of(s) else { continue };
        for u in &sc.uses {
            let e = acc.entry(u.full.to_string()).or_default();
            e.scripts_raw += 1;
            e.sites_call += u64::from(u.calls);
            e.sites_value += u64::from(u.values);
            e.sites_type += u64::from(u.types);
            e.uses_raw += s.uses;
        }
        for m in &sc.methods_unresolved {
            let e = acc.entry(m.clone()).or_default();
            e.scripts_raw += 1;
            e.sites_call += 1;
            e.uses_raw += s.uses;
        }
    }
    // Dedup universe: Fork representatives.
    for i in a.reps() {
        let s = &a.scripts[i];
        let Some(sc) = a.scan_of(s) else { continue };
        let names: Vec<String> = sc
            .uses
            .iter()
            .map(|u| u.full.to_string())
            .chain(sc.methods_unresolved.iter().cloned())
            .collect();
        for n in names {
            let Some(e) = acc.get_mut(&n) else { continue };
            e.scripts_dedup += 1;
            match sc.kind {
                Some(pine_ir::ScriptKind::Strategy) => e.in_strategies += 1,
                Some(pine_ir::ScriptKind::Indicator) => e.in_indicators += 1,
                _ => {}
            }
            match sc.dialect {
                Some(pine_ir::Dialect::V5) => e.v5 += 1,
                Some(pine_ir::Dialect::V6) => e.v6 += 1,
                None => {}
            }
            e.tops.push((s.score, s.rec.core.slug.clone()));
        }
        if sc.unknown.is_empty() && s.reach.unresolved.is_empty() && s.unavail.len() == 1 {
            if let Some(only) = s.unavail.iter().next() {
                if let Some(e) = acc.get_mut(*only) {
                    e.blocking_only += 1;
                }
            }
        }
    }
    for &li in a.lib_scans.values() {
        for u in &a.scans[li].uses {
            if let Some(e) = acc.get_mut(u.full) {
                e.in_libs += 1;
            }
        }
    }
    finish(a, acc)
}

fn finish(a: &Analysis, acc: BTreeMap<String, Acc>) -> Vec<BuiltinRow> {
    let mut entries: Vec<(String, Acc)> = Vec::new();
    let mut qs: Vec<(String, Acc)> = Vec::new();
    for (k, v) in acc {
        if k.starts_with("?.") {
            qs.push((k, v));
        } else {
            entries.push((k, v));
        }
    }
    entries.sort_by(|(ka, a1), (kb, b1)| {
        b1.scripts_dedup
            .cmp(&a1.scripts_dedup)
            .then(b1.scripts_raw.cmp(&a1.scripts_raw))
            .then(ka.cmp(kb))
    });
    let mut rows = Vec::new();
    let row = |full: &str, mut v: Acc, rank: Option<u32>| -> BuiltinRow {
        v.tops.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
        v.tops.truncate(3);
        let (ns, name, tier, scope, unavail) = match crate::spec_view::lookup(full) {
            Some(e) if rank.is_some() => (
                e.ns().to_string(),
                e.name().to_string(),
                Some(e.tier()),
                Some(if e.declined() { "declined" } else { "in" }.to_string()),
                Some(crate::build::unavailable(full, a.tiers)),
            ),
            _ => (
                "?".to_string(),
                full.trim_start_matches("?.").to_string(),
                None,
                None,
                None,
            ),
        };
        BuiltinRow {
            full: full.to_string(),
            ns,
            name,
            tier,
            scope,
            unavailable: unavail,
            rank,
            scripts_raw: v.scripts_raw,
            scripts_dedup: v.scripts_dedup,
            sites_call: v.sites_call,
            sites_value: v.sites_value,
            sites_type: v.sites_type,
            uses_raw: v.uses_raw,
            in_strategies: v.in_strategies,
            in_indicators: v.in_indicators,
            in_libs: v.in_libs,
            v5: v.v5,
            v6: v.v6,
            blocking_only: v.blocking_only,
            top_slugs: v.tops.into_iter().map(|(_, s)| s).collect(),
            corpus_sha256: a.corpus_sha256,
            spec_sha256: a.spec_sha256,
            tiers: a.tiers,
        }
    };
    for (r, (k, v)) in entries.into_iter().enumerate() {
        rows.push(row(&k, v, Some(r as u32 + 1)));
    }
    for (k, v) in qs {
        rows.push(row(&k, v, None));
    }
    rows
}

/// The second, independent count (I28): the same columns from `Scan.uses` in **slug** order,
/// one script at a time, with the representative set taken as a set.
fn builtin_rows_second(a: &Analysis) -> Vec<BuiltinRow> {
    let reps: BTreeSet<usize> = a.reps().into_iter().collect();
    let mut order: Vec<usize> = a.members.clone();
    order.sort_by(|&x, &y| a.scripts[x].rec.core.slug.cmp(&a.scripts[y].rec.core.slug));
    let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
    for &i in &order {
        let s = &a.scripts[i];
        let Some(sc) = a.scan_of(s) else { continue };
        let is_rep = reps.contains(&i);
        let touch =
            |name: String, calls: u64, values: u64, types: u64, acc: &mut BTreeMap<String, Acc>| {
                let e = acc.entry(name).or_default();
                e.scripts_raw += 1;
                e.sites_call += calls;
                e.sites_value += values;
                e.sites_type += types;
                e.uses_raw += s.uses;
                if is_rep {
                    e.scripts_dedup += 1;
                    if sc.kind == Some(pine_ir::ScriptKind::Strategy) {
                        e.in_strategies += 1;
                    }
                    if sc.kind == Some(pine_ir::ScriptKind::Indicator) {
                        e.in_indicators += 1;
                    }
                    if sc.dialect == Some(pine_ir::Dialect::V5) {
                        e.v5 += 1;
                    }
                    if sc.dialect == Some(pine_ir::Dialect::V6) {
                        e.v6 += 1;
                    }
                    e.tops.push((s.score, s.rec.core.slug.clone()));
                }
            };
        for u in &sc.uses {
            touch(
                u.full.to_string(),
                u64::from(u.calls),
                u64::from(u.values),
                u64::from(u.types),
                &mut acc,
            );
        }
        for m in &sc.methods_unresolved {
            touch(m.clone(), 1, 0, 0, &mut acc);
        }
    }
    for &i in &reps {
        let s = &a.scripts[i];
        let clean =
            a.scan_of(s).is_some_and(|sc| sc.unknown.is_empty()) && s.reach.unresolved.is_empty();
        if clean && s.unavail.len() == 1 {
            for u in &s.unavail {
                if let Some(e) = acc.get_mut(*u) {
                    e.blocking_only += 1;
                }
            }
        }
    }
    for &li in a.lib_scans.values() {
        for u in &a.scans[li].uses {
            if let Some(e) = acc.get_mut(u.full) {
                e.in_libs += 1;
            }
        }
    }
    finish(a, acc)
}

/// The dedup unit of a script (§3.5.5): its Fork cluster if it parsed, else its source
/// `sha256`, else its slug.
fn unit(a: &Analysis, i: usize) -> String {
    let s = &a.scripts[i];
    if s.parse_ok {
        if let Some(h) = s.fork {
            return format!("f:{}", crate::hex64(&h));
        }
    }
    match s.rec.core.sha256 {
        Some(h) => format!("s:{}", crate::hex64(&h)),
        None => format!("g:{}", s.rec.core.slug),
    }
}

/// Every `(code, detail)` a script carries (§4.11.3).
pub(crate) fn script_codes(a: &Analysis, i: usize) -> BTreeSet<(u16, String)> {
    let s = &a.scripts[i];
    let mut out = BTreeSet::new();
    let total = |out: &mut BTreeSet<(u16, String)>, c: DiagCode, d: Option<&str>| {
        out.insert((c.0, String::new()));
        if let Some(d) = d {
            out.insert((c.0, d.to_string()));
        }
    };
    let st = s.rec.core.status.as_str();
    if matches!(st, "listed" | "fetched") {
        // pending: no code (§4.11.4)
    } else if st != "parsed" || s.scan.is_none() {
        let st = if st == "parsed" { "no_source" } else { st };
        total(&mut out, codes::SOURCE_UNAVAILABLE, Some(st));
    } else if let Some(sc) = a.scan_of(s) {
        for &c in &sc.parse_codes {
            total(&mut out, c, None);
        }
        if s.parse_ok {
            for u in &s.unavail {
                total(&mut out, codes::BUILTIN_UNAVAILABLE, Some(u));
            }
            for n in &sc.unknown {
                total(&mut out, codes::BUILTIN_UNKNOWN, Some(n));
            }
            for d in &s.reach.unresolved {
                total(&mut out, codes::IMPORT_UNRESOLVED, Some(d));
            }
        }
    }
    for l in &s.licence {
        total(&mut out, codes::LICENCE_RESTRICTED, Some(l));
    }
    out
}

fn bucket_rows(a: &Analysis) -> Vec<BucketRow> {
    let mut acc: BTreeMap<(u16, String), (u32, BTreeSet<String>, u64)> = BTreeMap::new();
    for i in 0..a.scripts.len() {
        for k in script_codes(a, i) {
            let e = acc.entry(k).or_insert((0, BTreeSet::new(), 0));
            e.0 += 1;
            e.1.insert(unit(a, i));
            e.2 += a.scripts[i].uses;
        }
    }
    acc.into_iter()
        .map(|((code, detail), (raw, units, uses))| BucketRow {
            code,
            range: range_name(DiagCode(code)).to_string(),
            detail,
            scripts_raw: raw,
            scripts_dedup: units.len() as u32,
            uses_raw: uses,
            corpus_sha256: a.corpus_sha256,
            spec_sha256: a.spec_sha256,
            tiers: a.tiers,
        })
        .collect()
}

/// `(builtins.jsonl, buckets.jsonl)` bytes. The second count must agree (I28); a disagreement is
/// an internal error and panics, which aborts the process with a non-zero status (exit 1).
pub(crate) fn bytes(a: &Analysis) -> (Vec<u8>, Vec<u8>) {
    let first = builtin_rows(a);
    let second = builtin_rows_second(a);
    assert_eq!(
        first, second,
        "I28: builtins.jsonl disagrees with its second count"
    );
    let mut b = Vec::new();
    for r in &first {
        r.write(&mut b);
    }
    let mut k = Vec::new();
    for r in bucket_rows(a) {
        r.write(&mut k);
    }
    (b, k)
}

/// The §4.11.4 conservation terms: `[pending, unavailable_src, parse_failed, Σ Fork sizes]`.
pub(crate) fn conservation(a: &Analysis) -> [u32; 4] {
    let mut c = [0u32; 4];
    for s in &a.scripts {
        if matches!(s.rec.core.status.as_str(), "listed" | "fetched") {
            c[0] += 1;
        } else if s.rec.core.status != "parsed" || s.scan.is_none() {
            c[1] += 1;
        } else if !s.parse_ok {
            c[2] += 1;
        }
    }
    c[3] = a.fork.clusters.iter().map(|x| x.members.len() as u32).sum();
    c
}

/// `pine-corpus hist`: run the analysis and write the two histogram files.
pub fn run(corpus: &crate::layout::Corpus, tiers: u8) -> Result<(), CorpusError> {
    let a = crate::build::analyse(corpus, tiers, &crate::rank::Weights::default(), None)?;
    let (b, k) = bytes(&a);
    let out = corpus.out_dir();
    crate::write_atomic(&out.join("builtins.jsonl"), &b)?;
    crate::write_atomic(&out.join("buckets.jsonl"), &k)?;
    Ok(())
}
