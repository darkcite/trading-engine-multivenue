// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `schema.rs` (I19, I20), with AMEND-harvest's appended keys.

use pine_corpus::schema::*;

fn keys_of(line: &[u8]) -> Vec<String> {
    pine_corpus::jsonl::parse_line(line)
        .unwrap()
        .fields
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

fn sample_lines() -> Vec<(Vec<u8>, &'static [&'static str])> {
    let mut v = Vec::new();
    let mut o = Vec::new();
    IndexRow::default().write(&mut o);
    v.push((o, &INDEX_KEYS[..]));
    let mut o = Vec::new();
    DedupRow {
        profile: "fork".into(),
        hash: [1; 32],
        fork: None,
        size: 1,
        n_authors: 1,
        rep: "a".into(),
        rep_sha256: [2; 32],
        kind: None,
        dialect: 5,
        eligible: true,
        aliases: vec![],
        canon: 1,
        corpus_sha256: [3; 32],
        spec_sha256: [4; 32],
        tiers: 7,
    }
    .write(&mut o);
    v.push((o, &DEDUP_KEYS[..]));
    let mut o = Vec::new();
    BuiltinRow::default().write(&mut o);
    v.push((o, &BUILTIN_KEYS[..]));
    let mut o = Vec::new();
    BucketRow {
        code: 7101,
        range: "harness".into(),
        detail: String::new(),
        scripts_raw: 0,
        scripts_dedup: 0,
        uses_raw: 0,
        corpus_sha256: [0; 32],
        spec_sha256: [0; 32],
        tiers: 7,
    }
    .write(&mut o);
    v.push((o, &BUCKET_KEYS[..]));
    let mut o = Vec::new();
    TopnRow {
        rank: 1,
        score_1e6: 0,
        core: IndexCore::default(),
        kind: "strategy".into(),
        dialect: 6,
        cluster: [0; 32],
        cluster_size: 1,
        aliases: vec![],
        n_authors: 1,
        libs: vec![],
        n_inputs: 0,
        k_numeric: 0,
        k_dead: 0,
        k_options: 0,
        k_bounded: 0,
        plot_top: 0,
        plot_nested: 0,
        hline: 0,
        canon: 1,
        corpus_sha256: [0; 32],
        spec_sha256: [0; 32],
        tiers: 7,
        asof_ms: 0,
        weights: pine_corpus::rank::Weights::default(),
        acq: Some("manual".into()),
        licence_class: Some("mpl2".into()),
    }
    .write(&mut o);
    v.push((o, &TOPN_KEYS[..]));
    let mut o = Vec::new();
    LibsRow {
        rec: 1,
        user: "u".into(),
        lib: "l".into(),
        ver: 1,
        outcome: "ok".into(),
        http_status: None,
        retrieved_ms: None,
        sha256: None,
        attempts: 0,
        deferred_days: 0,
        licence_class: None,
    }
    .write(&mut o);
    v.push((o, &LIBS_KEYS[..]));
    let mut o = Vec::new();
    WantRow {
        user: "u".into(),
        lib: "l".into(),
        ver: 1,
        depth: 1,
        dependents: 1,
    }
    .write(&mut o);
    v.push((o, &WANT_KEYS[..]));
    v
}

#[test]
fn schema_pins_the_schema_1_key_lists() {
    // §3.5 as amended, literally.
    let index: [&str; 25] = [
        "schema",
        "rec",
        "slug",
        "url",
        "title",
        "author",
        "listed_type",
        "open_source",
        "published_ms",
        "updated_ms",
        "likes",
        "boosts",
        "uses",
        "listed_ms",
        "status",
        "http_status",
        "raw_sha256",
        "retrieved_ms",
        "sha256",
        "src_bytes",
        "version_src",
        "version_page",
        "licence_override",
        "acq",
        "licence_class",
    ];
    assert_eq!(INDEX_KEYS, index);
    assert_eq!(
        DEDUP_KEYS,
        [
            "schema",
            "profile",
            "hash",
            "fork",
            "size",
            "n_authors",
            "rep",
            "rep_sha256",
            "kind",
            "dialect",
            "eligible",
            "aliases",
            "canon",
            "corpus_sha256",
            "spec_sha256",
            "tiers"
        ]
    );
    assert_eq!(
        BUILTIN_KEYS,
        [
            "schema",
            "full",
            "ns",
            "name",
            "tier",
            "scope",
            "unavailable",
            "rank",
            "scripts_raw",
            "scripts_dedup",
            "sites_call",
            "sites_value",
            "sites_type",
            "uses_raw",
            "in_strategies",
            "in_indicators",
            "in_libs",
            "v5",
            "v6",
            "blocking_only",
            "top_slugs",
            "corpus_sha256",
            "spec_sha256",
            "tiers"
        ]
    );
    assert_eq!(
        BUCKET_KEYS,
        [
            "schema",
            "code",
            "range",
            "detail",
            "scripts_raw",
            "scripts_dedup",
            "uses_raw",
            "corpus_sha256",
            "spec_sha256",
            "tiers"
        ]
    );
    assert_eq!(
        LIBS_KEYS,
        [
            "schema",
            "rec",
            "user",
            "lib",
            "ver",
            "outcome",
            "http_status",
            "retrieved_ms",
            "sha256",
            "attempts",
            "deferred_days",
            "licence_class"
        ]
    );
    assert_eq!(
        WANT_KEYS,
        ["schema", "user", "lib", "ver", "depth", "dependents"]
    );
    assert_eq!(TOPN_KEYS.len(), 53);
    assert_eq!(&TOPN_KEYS[51..], ["acq", "licence_class"]);
    for (line, keys) in sample_lines() {
        assert_eq!(keys_of(&line), keys.to_vec());
    }
}

#[test]
fn schema_every_writer_emits_schema_one_first() {
    for (line, _) in sample_lines() {
        assert!(line.starts_with(b"{\"schema\":1,"));
        assert!(line.ends_with(b"}\n"));
    }
}

#[test]
fn schema_no_market_data_field() {
    const BAD: [&str; 10] = [
        "open", "high", "low", "close", "volume", "ohlc", "ohlcv", "price", "bar", "bars",
    ];
    for keys in ALL_KEY_LISTS {
        for k in keys {
            if *k == "open_source" {
                continue;
            }
            for tok in k.split('_') {
                assert!(!BAD.contains(&tok), "market-data token in key `{k}`");
            }
        }
    }
}

#[test]
fn schema_topn_inlines_index_fields_under_the_same_names() {
    assert_eq!(&TOPN_KEYS[3..24], &INDEX_KEYS[2..23]);
}

#[test]
fn schema_reader_refuses_a_schema_other_than_one() {
    assert!(WantRow::read(
        b"{\"schema\":2,\"user\":\"u\",\"lib\":\"l\",\"ver\":1,\"depth\":1,\"dependents\":1}"
    )
    .is_err());
    assert!(
        WantRow::read(b"{\"schema\":1,\"user\":\"u\",\"lib\":\"l\",\"ver\":1,\"depth\":1}")
            .is_err()
    );
}

#[test]
fn schema_appended_keys_read_as_null_when_absent() {
    let mut o = Vec::new();
    LibsRow {
        rec: 1,
        user: "u".into(),
        lib: "l".into(),
        ver: 1,
        outcome: "ok".into(),
        http_status: None,
        retrieved_ms: None,
        sha256: None,
        attempts: 0,
        deferred_days: 0,
        licence_class: Some("mpl2".into()),
    }
    .write(&mut o);
    let s = String::from_utf8(o)
        .unwrap()
        .replace(",\"licence_class\":\"mpl2\"", "");
    assert_eq!(LibsRow::read(s.as_bytes()).unwrap().licence_class, None);
}
