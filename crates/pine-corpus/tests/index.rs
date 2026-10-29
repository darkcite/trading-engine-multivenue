// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `index.rs` (I7, §3.5.10).

mod common;

use std::path::Path;

use pine_corpus::index::{corpus_sha256, load_bytes, Index};

fn p() -> &'static Path {
    Path::new("index.jsonl")
}

fn fixture_bytes() -> Vec<u8> {
    std::fs::read(common::fixtures().join("index.fixture.jsonl")).unwrap()
}

#[test]
fn index_truncated_final_line_is_dropped() {
    let mut b = fixture_bytes();
    b.extend_from_slice(b"{\"schema\":1,\"rec\":9,\"slug\":\"slug-c\"");
    let i = load_bytes(&b, p()).unwrap();
    assert_eq!(i.records.len(), 2);
}

#[test]
fn index_non_final_corrupt_line_is_an_error() {
    let mut b = b"{\"schema\":1,\"rec\":\n".to_vec();
    b.extend_from_slice(&fixture_bytes());
    assert!(matches!(
        load_bytes(&b, p()),
        Err(pine_corpus::CorpusError::Row { line: 1, .. })
    ));
}

#[test]
fn index_last_rec_wins_per_slug() {
    let i = load_bytes(&fixture_bytes(), p()).unwrap();
    let a = i.records.iter().find(|r| r.core.slug == "slug-a").unwrap();
    assert_eq!(a.rec, 3);
    assert_eq!(a.core.title, "A new");
    assert_eq!(i.max_rec, 3);
}

#[test]
fn index_schema_other_than_one_is_refused() {
    let b = String::from_utf8(fixture_bytes())
        .unwrap()
        .replacen("\"schema\":1", "\"schema\":2", 1);
    assert!(load_bytes(b.as_bytes(), p()).is_err());
}

#[test]
fn index_corpus_sha256_matches_the_vectors() {
    assert_eq!(
        pine_corpus::hex64(&corpus_sha256(&Index::default())),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let i = load_bytes(&fixture_bytes(), p()).unwrap();
    assert_eq!(
        pine_corpus::hex64(&corpus_sha256(&i)),
        "0a5f4f34f67fb705e1e7f6ac501dac1840ebfab8f030d8fd76b2307350def6ab"
    );
}

#[test]
fn index_corpus_sha256_is_invariant_under_compaction_and_line_order() {
    let b = fixture_bytes();
    let lines: Vec<&[u8]> = b.split_inclusive(|&c| c == b'\n').collect();
    let mut rev: Vec<u8> = Vec::new();
    // Reverse order keeps rec 3 winning for slug-a.
    for l in lines.iter().rev() {
        rev.extend_from_slice(l);
    }
    let a = corpus_sha256(&load_bytes(&b, p()).unwrap());
    let r = corpus_sha256(&load_bytes(&rev, p()).unwrap());
    // Compaction: only the effective records.
    let i = load_bytes(&b, p()).unwrap();
    let mut compact = Vec::new();
    for rec in &i.records {
        rec.write(&mut compact);
    }
    let c = corpus_sha256(&load_bytes(&compact, p()).unwrap());
    assert_eq!(a, r);
    assert_eq!(a, c);
}

#[test]
fn index_reader_agrees_with_the_shared_fixture() {
    let i = load_bytes(&fixture_bytes(), p()).unwrap();
    let slugs: Vec<&str> = i.records.iter().map(|r| r.core.slug.as_str()).collect();
    assert_eq!(slugs, ["slug-a", "slug-b"]);
    assert_eq!(i.records[0].core.uses, Some(5));
    assert_eq!(i.records[1].core.uses, None);
    assert_eq!(i.records[0].acq.as_deref(), Some("manual"));
    let libs =
        pine_corpus::index::load_libs(&common::fixtures().join("libs.fixture.jsonl")).unwrap();
    assert_eq!(libs.len(), 2);
    assert_eq!(libs[1].outcome, "lib_missing");
}
