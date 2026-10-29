// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `verify.rs` (I8, I28).

mod common;

use common::{corpus, ind, S};

#[test]
fn verify_src_name_mismatch_is_refused() {
    let (_t, c) = corpus("mismatch", &[S::new("a", &ind("plot(close)"))], &[]);
    let idx = pine_corpus::index::load(&c.index_path()).unwrap();
    let p = c.src_path(&idx.records[0].core.sha256.unwrap());
    std::fs::write(&p, "edited").unwrap();
    assert!(matches!(
        pine_corpus::verify::run(&c),
        Err(pine_corpus::CorpusError::ShaMismatch { .. })
    ));
}

#[test]
fn verify_parsed_record_without_src_is_reported() {
    let (_t, c) = corpus("nosrc", &[S::new("a", &ind("plot(close)"))], &[]);
    let idx = pine_corpus::index::load(&c.index_path()).unwrap();
    std::fs::remove_file(c.src_path(&idx.records[0].core.sha256.unwrap())).unwrap();
    let r = pine_corpus::verify::run(&c).unwrap();
    assert_eq!(r.missing, vec![("a".to_string(), "src")]);
    assert!(!r.clean());
}

#[test]
fn verify_reports_the_conservation_identity() {
    let (_t, c) = corpus(
        "cons",
        &[
            S::new("a", &ind("plot(close)")),
            S::new("b", &ind("plot(open)")),
            S::new("c", "//@version=4\nstudy(\"t\")\n"),
        ],
        &[],
    );
    let r = pine_corpus::verify::run(&c).unwrap();
    assert_eq!(r.conservation, [0, 0, 1, 2]);
    assert_eq!(r.records, 3);
    assert!(r.clean());
    assert_eq!(r.sources, 3);
}
