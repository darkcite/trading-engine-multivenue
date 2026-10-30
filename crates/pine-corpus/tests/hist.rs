// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `hist.rs` (I21, I28, §4.11).

mod common;

use common::{corpus, fixture, ind, opts, out, S};
use pine_corpus::schema::{BucketRow, BuiltinRow};

fn builtins(c: &pine_corpus::layout::Corpus) -> Vec<BuiltinRow> {
    out(c, "builtins.jsonl")
        .split_inclusive(|&b| b == b'\n')
        .map(|l| BuiltinRow::read(l).unwrap())
        .collect()
}

fn buckets(c: &pine_corpus::layout::Corpus) -> Vec<BucketRow> {
    out(c, "buckets.jsonl")
        .split_inclusive(|&b| b == b'\n')
        .map(|l| BucketRow::read(l).unwrap())
        .collect()
}

fn mixed() -> Vec<S> {
    vec![
        S::new("a", &fixture("fork_b01.pine")).uses(5),
        S::new("a2", &fixture("fork_b01_v1.pine")).uses(1),
        S::new("b", &fixture("fork_b06.pine")).uses(7),
        S::new("t4", &ind("x = line.new(0, 0, 1, 1)\nplot(close)")).uses(2),
        S::new(
            "t44",
            &ind("x = line.new(0, 0, 1, 1)\ny = label.new(0, 0)\nplot(close)"),
        )
        .uses(2),
        S::new("bad", "//@version=5\nindicator(\"t\"\nplot(\n").uses(3),
        S::new("v4", "//@version=4\nstudy(\"t\")\nplot(close)\n"),
        S::new("gone", "").status("gone"),
        S::new("unk", &ind("plot(nosuch(close))")),
        S::new("q", &ind("f() => 1\nx = f()\nx.push(1)\nplot(close)")),
    ]
}

#[test]
fn hist_builtin_counts_conserve_against_a_second_pass() {
    // `build` asserts the second count internally (I28); a disagreement panics.
    let (_t, c) = corpus("second", &mixed(), &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert!(!builtins(&c).is_empty());
}

#[test]
fn hist_conservation_identity_holds() {
    let (_t, c) = corpus("conserve", &mixed(), &[]);
    let r = pine_corpus::verify::run(&c).unwrap();
    assert_eq!(r.conservation.iter().sum::<u32>(), r.records);
    assert_eq!(r.records, 10);
    assert_eq!(r.conservation[1], 1); // gone
    assert_eq!(r.conservation[2], 2); // bad + v4
}

#[test]
fn hist_every_bucket_code_is_a_parse_code_or_a_corpus_code() {
    let (_t, c) = corpus("codes", &mixed(), &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    for b in buckets(&c) {
        let code = pine_ir::DiagCode(b.code);
        assert!(
            pine_lang::code::ALL.contains(&code)
                || pine_corpus::codes::ALL.contains(&code)
                || b.code == 3001,
            "{}",
            b.code
        );
        assert_eq!(b.range, pine_corpus::hist::range_name(code));
    }
}

#[test]
fn hist_never_emits_a_sema_code_other_than_3001() {
    let (_t, c) = corpus("sema", &mixed(), &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    for b in buckets(&c) {
        if (3000..4000).contains(&b.code) {
            assert_eq!(b.code, 3001);
        }
    }
    assert!(buckets(&c).iter().any(|b| b.code == 3001));
}

#[test]
fn hist_blocking_only_counts_scripts_whose_only_unavailable_entry_is_this_one() {
    let (_t, c) = corpus("blocking", &mixed(), &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let rows = builtins(&c);
    let line = rows.iter().find(|r| r.full == "line.new").unwrap();
    // `t4` is blocked by line.new alone; `t44` needs label.new too and counts toward neither.
    assert_eq!(line.blocking_only, 1);
    let label = rows.iter().find(|r| r.full == "label.new").unwrap();
    assert_eq!(label.blocking_only, 0);
    assert_eq!(line.unavailable, Some(true));
    assert_eq!(line.tier, Some(4));
}

#[test]
fn hist_library_references_count_toward_blocking() {
    let lib =
        "//@version=5\nlibrary(\"L\")\nexport f(float x) =>\n    line.new(0, 0, 1, 1)\n    x\n";
    let v = vec![S::new(
        "user",
        "//@version=5\nindicator(\"t\")\nimport zed/L/1 as l\nplot(l.f(close))\n",
    )];
    let (_t, c) = corpus("libblock", &v, &[("zed/L/1", lib)]);
    let s = pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    assert!(s.excluded.iter().any(|(c, n)| c.0 == 7101 && *n == 1));
    let b = buckets(&c);
    assert!(b.iter().any(|r| r.code == 7101 && r.detail == "line.new"));
    // Under tiers 0b1111 the library no longer blocks.
    let s = pine_corpus::build::build(&c, &opts(0b1111)).unwrap();
    assert!(s.excluded.is_empty(), "{:?}", s.excluded);
}

#[test]
fn hist_rank_orders_by_dedup_then_raw_then_name() {
    let (_t, c) = corpus("order", &mixed(), &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let rows: Vec<BuiltinRow> = builtins(&c)
        .into_iter()
        .filter(|r| r.rank.is_some())
        .collect();
    for w in rows.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        assert!(
            (a.scripts_dedup, a.scripts_raw) > (b.scripts_dedup, b.scripts_raw)
                || ((a.scripts_dedup, a.scripts_raw) == (b.scripts_dedup, b.scripts_raw)
                    && a.full < b.full)
        );
        assert_eq!(b.rank.unwrap(), a.rank.unwrap() + 1);
    }
    let all = builtins(&c);
    let q = all.iter().position(|r| r.full.starts_with("?.")).unwrap();
    assert!(all[q..].iter().all(|r| r.rank.is_none()));
}

#[test]
fn hist_counts_are_reported_over_raw_and_dedup() {
    let (_t, c) = corpus("rawdedup", &mixed(), &[]);
    pine_corpus::build::build(&c, &opts(0b0111)).unwrap();
    let rows = builtins(&c);
    for r in &rows {
        assert!(r.scripts_dedup <= r.scripts_raw, "{}", r.full);
    }
    let sma = rows.iter().find(|r| r.full == "ta.sma").unwrap();
    assert_eq!(sma.scripts_raw, 2); // a and its fork a2
    assert_eq!(sma.scripts_dedup, 1);
}
