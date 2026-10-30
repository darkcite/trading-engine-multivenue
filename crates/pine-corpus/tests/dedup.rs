// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `dedup.rs` (I22, I26, D9-g as amended).

mod common;

use common::{corpus, fixture, opts, out, S};
use pine_corpus::dedup::{cluster, collapse_ppm, member_order, Member};
use pine_corpus::schema::DedupRow;
use pine_corpus::Profile;

fn m(slug: &str, eligible: bool, score: i64, uses: u64, pub_ms: Option<i64>) -> Member<'_> {
    Member {
        slug,
        hash: [7; 32],
        eligible,
        score_1e6: score,
        uses,
        published_ms: pub_ms,
    }
}

#[test]
fn dedup_representative_order_is_total() {
    let ms = [
        m("a", true, 5, 1, Some(2)),
        m("b", true, 5, 1, Some(2)),
        m("c", true, 5, 1, None),
        m("d", false, 9, 9, Some(1)),
    ];
    for x in &ms {
        for y in &ms {
            if x.slug != y.slug {
                assert_ne!(member_order(x, y), std::cmp::Ordering::Equal);
            }
        }
    }
    let c = cluster(&ms, Profile::Fork);
    let order: Vec<&str> = c.clusters[0]
        .members
        .iter()
        .map(|&i| ms[i as usize].slug)
        .collect();
    assert_eq!(order, ["a", "b", "c", "d"]);
}

#[test]
fn dedup_eligible_member_is_preferred_as_representative() {
    let ms = [
        m("hot", false, 99, 99, Some(1)),
        m("ok", true, 1, 1, Some(2)),
    ];
    let c = cluster(&ms, Profile::Fork);
    assert_eq!(ms[c.clusters[0].members[0] as usize].slug, "ok");
}

fn fork_corpus(tag: &str) -> (common::Tmp, pine_corpus::layout::Corpus, usize) {
    let mut v = Vec::new();
    for b in 1..=12 {
        v.push(S::new(&format!("b{b:02}"), &fixture(&format!("fork_b{b:02}.pine"))).uses(100));
        for k in 1..=2 {
            v.push(
                S::new(
                    &format!("b{b:02}v{k}"),
                    &fixture(&format!("fork_b{b:02}_v{k}.pine")),
                )
                .uses(10 * k as u64)
                .author(&format!("au{k}")),
            );
        }
    }
    for n in 1..=4 {
        v.push(S::new(
            &format!("n{n}a"),
            &fixture(&format!("nonfork_n{n}_a.pine")),
        ));
        v.push(S::new(
            &format!("n{n}b"),
            &fixture(&format!("nonfork_n{n}_b.pine")),
        ));
    }
    let n = v.len();
    let (t, c) = corpus(tag, &v, &[]);
    (t, c, n)
}

fn rows(c: &pine_corpus::layout::Corpus) -> Vec<DedupRow> {
    out(c, "dedup.jsonl")
        .split_inclusive(|&b| b == b'\n')
        .map(|l| DedupRow::read(l).unwrap())
        .collect()
}

#[test]
fn dedup_aliases_are_recorded_not_deleted() {
    let (_t, c, n) = fork_corpus("aliases");
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let fork: Vec<DedupRow> = rows(&c)
        .into_iter()
        .filter(|r| r.profile == "fork")
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    for r in &fork {
        assert!(seen.insert(r.rep.clone()));
        for a in &r.aliases {
            assert!(seen.insert(a.clone()), "{a} twice");
        }
    }
    assert_eq!(seen.len(), n);
}

#[test]
fn dedup_two_runs_are_byte_identical() {
    let (_t, c, _) = fork_corpus("tworuns");
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let a = out(&c, "dedup.jsonl");
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert_eq!(a, out(&c, "dedup.jsonl"));
}

#[test]
fn dedup_output_is_invariant_under_index_line_order() {
    let (_t, c, _) = fork_corpus("lineorder");
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let a = out(&c, "dedup.jsonl");
    let b = out(&c, "builtins.jsonl");
    let ix = std::fs::read(c.index_path()).unwrap();
    let mut lines: Vec<&[u8]> = ix.split_inclusive(|&x| x == b'\n').collect();
    lines.reverse();
    std::fs::write(c.index_path(), lines.concat()).unwrap();
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert_eq!(a, out(&c, "dedup.jsonl"));
    assert_eq!(b, out(&c, "builtins.jsonl"));
}

#[test]
fn dedup_exact_clusters_nest_in_fork_clusters() {
    let (_t, c, _) = fork_corpus("nest");
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let all = rows(&c);
    let fork: std::collections::BTreeMap<[u8; 32], &DedupRow> = all
        .iter()
        .filter(|r| r.profile == "fork")
        .map(|r| (r.hash, r))
        .collect();
    for e in all.iter().filter(|r| r.profile == "exact") {
        let f = fork[&e.fork.unwrap()];
        let fm: std::collections::BTreeSet<&String> =
            std::iter::once(&f.rep).chain(f.aliases.iter()).collect();
        assert!(fm.contains(&e.rep));
        for a in &e.aliases {
            assert!(fm.contains(a));
        }
    }
}

#[test]
fn dedup_exact_cluster_members_have_identical_streams() {
    let (_t, c, _) = fork_corpus("streams");
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let idx = pine_corpus::index::load(&c.index_path()).unwrap();
    let src = |slug: &str| {
        let r = idx.records.iter().find(|r| r.core.slug == slug).unwrap();
        std::fs::read_to_string(c.src_path(&r.core.sha256.unwrap())).unwrap()
    };
    for r in rows(&c)
        .iter()
        .filter(|r| r.profile == "exact" && r.size > 1)
    {
        let mut a = Vec::new();
        pine_corpus::canon::stream(&src(&r.rep), Profile::Exact, &mut a).unwrap();
        for s in &r.aliases {
            let mut b = Vec::new();
            pine_corpus::canon::stream(&src(s), Profile::Exact, &mut b).unwrap();
            assert_eq!(a, b);
        }
    }
}

#[test]
fn dedup_collapse_ppm_matches_a_hand_count() {
    let (_t, c, n) = fork_corpus("handcount");
    assert_eq!(n, 44);
    let s = pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    // 12 fork clusters of 3, plus non-fork pairs: n1a≡b03 family? n1a is fork_b03's text, so
    // it joins b03's cluster; n3a is b01's text and joins b01's. Clusters: 12 + 6 = 18.
    let fork = rows(&c).into_iter().filter(|r| r.profile == "fork").count() as u32;
    assert_eq!(fork, 18);
    assert_eq!(s.collapse_ppm, collapse_ppm(18, 44));
    assert_eq!(collapse_ppm(18, 44), 590_909);
    assert_eq!(collapse_ppm(0, 0), 0);
    assert_eq!(collapse_ppm(1, 2), 500_000);
}

#[test]
fn dedup_canary_stops_build_with_exit_four() {
    // 200 copies of one script acquired outside the manual intake: collapse ≈ 995 000 ppm.
    let src = fixture("fork_b03.pine");
    let v: Vec<S> = (0..200)
        .map(|i| S::new(&format!("s{i:03}"), &src).acq("repo"))
        .collect();
    let (_t, c) = corpus("canary", &v, &[]);
    let e = pine_corpus::build::build(&c, &opts(0b0111)).unwrap_err();
    assert!(matches!(
        e,
        pine_corpus::CorpusError::Canary {
            collapse_ppm: 995_000
        }
    ));
    assert!(!out(&c, "dedup.jsonl").is_empty());
    assert!(out(&c, "builtins.jsonl").is_empty());
    assert!(out(&c, "buckets.jsonl").is_empty());
    assert!(out(&c, "topn-strategies.jsonl").is_empty());
}

#[test]
fn dedup_canary_is_reported_not_a_stop_under_the_manual_intake() {
    let src = fixture("fork_b03.pine");
    let v: Vec<S> = (0..200)
        .map(|i| S::new(&format!("s{i:03}"), &src))
        .collect();
    let (_t, c) = corpus("canary-manual", &v, &[]);
    let s = pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert!(s.canary_reported);
    assert!(!out(&c, "builtins.jsonl").is_empty());
}
