// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `rank.rs` (I29, I22, D9-i).

mod common;

use common::{corpus, fixture, ind, opts, out, S};
use pine_corpus::rank::{score_1e6, select, Candidate, Weights};
use pine_corpus::schema::TopnRow;

fn topn(c: &pine_corpus::layout::Corpus, f: &str) -> Vec<TopnRow> {
    out(c, f)
        .split_inclusive(|&b| b == b'\n')
        .map(|l| TopnRow::read(l).unwrap())
        .collect()
}

#[test]
fn rank_score_is_deterministic_and_integer_stable() {
    let w = Weights::default();
    let a = score_1e6(
        Some(100),
        Some(5),
        Some(30),
        3,
        Some(0),
        365 * 86_400_000,
        &w,
    );
    let b = score_1e6(
        Some(100),
        Some(5),
        Some(30),
        3,
        Some(0),
        365 * 86_400_000,
        &w,
    );
    assert_eq!(a, b);
    assert_eq!(score_1e6(None, None, None, 0, None, 0, &w), 0);
    // Freshness alone at age 0 is the weight itself.
    assert_eq!(score_1e6(None, None, None, 0, Some(0), 0, &w), 100_000);
    // Beyond the horizon freshness is 0.
    assert_eq!(
        score_1e6(None, None, None, 0, Some(0), 2000 * 86_400_000, &w),
        0
    );
}

#[test]
fn rank_ineligible_scripts_are_excluded_before_ranking() {
    let v = vec![
        S::new("ok", &ind("plot(close)")).uses(1),
        S::new("v4", "//@version=4\nstudy(\"t\")\nplot(close)\n").uses(1000),
        S::new("broken", "//@version=5\nindicator(\"t\"\nplot(\n").uses(1000),
        S::new("t4", &ind("x = line.new(0, 0, 1, 1)\nplot(close)")).uses(1000),
        S::new("unk", &ind("plot(nosuch(close))")).uses(1000),
        S::new(
            "imp",
            "//@version=5\nindicator(\"t\")\nimport zed/Gone/1 as g\nplot(g.f(close))\n",
        )
        .uses(1000),
        S::new("nc", &ind("plot(open)")).uses(1000).class("nc"),
        S::new("closed", "").status("closed").uses(1000),
        S::new("fork", &ind("plot(close)")).uses(0),
    ];
    let (_t, c) = corpus("inel", &v, &[]);
    let s = pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let rows = topn(&c, "topn-indicators.jsonl");
    let slugs: Vec<&str> = rows.iter().map(|r| r.core.slug.as_str()).collect();
    assert_eq!(slugs, ["ok"]);
    let ex: std::collections::BTreeMap<u16, u32> =
        s.excluded.iter().map(|(c, n)| (c.0, *n)).collect();
    assert_eq!(ex.get(&3001), Some(&1));
    assert_eq!(ex.get(&7101), Some(&1));
    assert_eq!(ex.get(&7102), Some(&1));
    assert_eq!(ex.get(&7103), Some(&1));
    assert_eq!(ex.get(&7104), Some(&1));
    assert_eq!(ex.get(&7105), Some(&1));
}

#[test]
fn rank_per_author_cap_is_applied_after_scoring() {
    let mut v = Vec::new();
    for i in 0..9 {
        v.push(
            S::new(&format!("p{i}"), &ind(&format!("plot(close * {i}.5)")))
                .author("prolific")
                .uses(1000 + i),
        );
    }
    v.push(S::new("other", &ind("plot(open)")).author("other").uses(1));
    let (_t, c) = corpus("cap", &v, &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let rows = topn(&c, "topn-indicators.jsonl");
    assert_eq!(
        rows.iter()
            .filter(|r| r.core.author.as_deref() == Some("prolific"))
            .count(),
        5
    );
    assert!(rows.iter().any(|r| r.core.slug == "other"));
    let cands: Vec<Candidate> = (0..9)
        .map(|i| Candidate {
            slug: format!("x{i}"),
            author: Some("a".into()),
            score_1e6: i,
            uses: 0,
            cluster_size: 1,
            published_ms: None,
        })
        .collect();
    let pick = select(&cands, Some(3), 5);
    assert_eq!(pick, vec![8, 7, 6]);
}

#[test]
fn rank_kinds_rank_independently() {
    let v = vec![
        S::new("i1", &ind("plot(close)")),
        S::new("i2", &ind("plot(open)")),
        S::new("s1", &fixture("fork_b01.pine")),
        S::new("s2", &fixture("fork_b04.pine")),
    ];
    let (_t, c) = corpus("kinds", &v, &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let s = topn(&c, "topn-strategies.jsonl");
    let i = topn(&c, "topn-indicators.jsonl");
    assert!(s.iter().all(|r| r.kind == "strategy"));
    assert!(i.iter().all(|r| r.kind == "indicator"));
    assert_eq!(s.iter().map(|r| r.rank).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(i.iter().map(|r| r.rank).collect::<Vec<_>>(), [1, 2]);
}

#[test]
fn rank_order_is_total() {
    let mk = |slug: &str, score, uses, size, p| Candidate {
        slug: slug.into(),
        author: None,
        score_1e6: score,
        uses,
        cluster_size: size,
        published_ms: p,
    };
    let c = [
        mk("b", 5, 1, 1, Some(1)),
        mk("a", 5, 1, 1, Some(1)),
        mk("c", 5, 1, 1, None),
        mk("d", 5, 1, 2, None),
        mk("e", 6, 0, 1, None),
    ];
    let pick = select(&c, None, 99);
    let s: Vec<&str> = pick.iter().map(|&i| c[i].slug.as_str()).collect();
    assert_eq!(s, ["e", "d", "a", "b", "c"]);
}

#[test]
fn rank_alias_count_raises_a_representative_above_an_unforked_peer() {
    let base = fixture("fork_b03.pine");
    let v = vec![
        S::new("forked", &base).uses(10),
        S::new("forked2", &fixture("fork_b03_v1.pine")).uses(0),
        S::new("forked3", &fixture("fork_b03_v2.pine")).uses(0),
        S::new("alone", &ind("plot(open * 2)")).uses(10),
    ];
    let (_t, c) = corpus("alias", &v, &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let rows = topn(&c, "topn-indicators.jsonl");
    assert_eq!(rows[0].core.slug, "forked");
    assert_eq!(rows[0].cluster_size, 3);
    assert!(rows[0].score_1e6 > rows[1].score_1e6);
}

#[test]
fn rank_asof_defaults_to_the_latest_retrieval() {
    let (_t, c) = corpus("asof", &[S::new("a", &ind("plot(close)"))], &[]);
    let s = pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert_eq!(s.asof_ms, 1_760_000_000_000);
}

#[test]
fn rank_rows_record_weights_and_provenance() {
    let (_t, c) = corpus(
        "prov",
        &[S::new("a", &ind("x = input.int(3)\nplot(close * x)"))],
        &[],
    );
    let s = pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let r = &topn(&c, "topn-indicators.jsonl")[0];
    assert_eq!(r.weights, Weights::default());
    assert_eq!(r.corpus_sha256, s.corpus_sha256);
    assert_eq!(r.canon, pine_corpus::CANON_VERSION);
    assert_eq!(r.tiers, 0b0111);
    assert_eq!(r.asof_ms, s.asof_ms);
    assert_eq!(r.n_inputs, 1);
    assert_eq!(r.k_numeric, 1);
    assert_eq!(r.acq.as_deref(), Some("manual"));
    assert_eq!(r.licence_class.as_deref(), Some("mpl2"));
}

#[test]
fn rank_two_runs_are_byte_identical() {
    let v = vec![
        S::new("a", &fixture("fork_b01.pine")),
        S::new("b", &fixture("fork_b02.pine")),
        S::new("c", &fixture("fork_b05.pine")),
    ];
    let (_t, c) = corpus("rank2", &v, &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let a = (
        out(&c, "topn-strategies.jsonl"),
        out(&c, "topn-indicators.jsonl"),
    );
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert_eq!(
        a,
        (
            out(&c, "topn-strategies.jsonl"),
            out(&c, "topn-indicators.jsonl")
        )
    );
}

#[test]
fn rank_weights_file_parses_and_refuses_unknown_keys() {
    let w = pine_corpus::rank::parse_weights("# c\nw_uses_ppm = 1\nmax_per_author=2\n").unwrap();
    assert_eq!(w.w_uses_ppm, 1);
    assert_eq!(w.max_per_author, 2);
    assert!(pine_corpus::rank::parse_weights("nope = 1").is_err());
}

#[test]
fn rank_licence_gate_covers_every_transitive_library() {
    // The script's own class is admitted; a library it reaches is `nc` → 7105 `lib:nc`.
    let lib = "//@version=5\nlibrary(\"L\")\nexport f(float x) =>\n    x\n";
    let v = vec![S::new(
        "user",
        "//@version=5\nindicator(\"t\")\nimport zed/L/1 as l\nplot(l.f(close))\n",
    )];
    let (_t, c) = corpus("libgate", &v, &[("zed/L/1", lib)]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert_eq!(topn(&c, "topn-indicators.jsonl").len(), 1);
    // Re-classify the library `nc` (a later libs.jsonl record wins).
    let mut b = std::fs::read(c.libs_path()).unwrap();
    let rows = pine_corpus::index::load_libs(&c.libs_path()).unwrap();
    let mut r = rows[0].clone();
    r.rec = 9;
    r.licence_class = Some("nc".into());
    r.write(&mut b);
    std::fs::write(c.libs_path(), b).unwrap();
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert!(topn(&c, "topn-indicators.jsonl").is_empty());
    let buckets = String::from_utf8(out(&c, "buckets.jsonl")).unwrap();
    assert!(
        buckets.contains("\"code\":7105,\"range\":\"harness\",\"detail\":\"lib:nc\""),
        "{buckets}"
    );
}
