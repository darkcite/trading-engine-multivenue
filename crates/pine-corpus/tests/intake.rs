// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! P2 of the harvest plan (R-P33): the manual intake — `import [--check]`, `import-lib`, the
//! TSV reader, the licence class table, the date rule and slug derivation; I17, I33–I37 on
//! synthetic rows. Every script here is hand-written (`OUR CODE`).

mod common;

use std::path::PathBuf;

use common::Tmp;
use pine_corpus::intake::{self, date_rule, parse_date, parse_utc_minute, slug_of, Source};
use pine_corpus::layout::Corpus;
use pine_corpus::licence;

const SESSION: &str = "h20261029T1000";
const HEADER: &str = "session\tfile\turl\ttitle\tauthor\tkind\tvisibility\tpublished\tupdated\tpine_version\tlicence_line\tlicence_class\tcopied_utc\thow\tlikes\tboosts\tuses\tnotes";
const LIB_HEADER: &str = "session\tfile\turl\ttitle\tauthor\tkind\tvisibility\tpublished\tupdated\tpine_version\tlicence_line\tlicence_class\tcopied_utc\thow\tnotes\tuser\tlib\tver\toutcome";

struct Env {
    _t: Tmp,
    vault: PathBuf,
    intake: PathBuf,
    corpus: Corpus,
}

fn env(tag: &str) -> Env {
    let t = Tmp::new(tag);
    let vault = t.path().join("vault");
    let intake = t.path().join("intake");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::create_dir_all(intake.join(SESSION)).unwrap();
    std::fs::write(
        vault.join(format!("session-{SESSION}.md")),
        "# session\n- start: 2026-10-29T10:00Z\n- end: 2026-10-29T11:30Z\n",
    )
    .unwrap();
    let root = t.path().join("corpus");
    std::fs::create_dir_all(&root).unwrap();
    let corpus = Corpus::open(&root).unwrap();
    Env {
        _t: t,
        vault,
        intake,
        corpus,
    }
}

fn script(body: &str) -> String {
    format!("//@version=5\n// OUR CODE — not harvested\n{body}\n")
}

/// A ledger row; `f` overrides columns by name.
fn row(file: &str, id: &str, f: &[(&str, &str)]) -> String {
    let mut cols: Vec<(&str, String)> = vec![
        ("session", SESSION.into()),
        ("file", file.into()),
        (
            "url",
            format!("https://example.invalid/script/{id}-some-title/"),
        ),
        ("title", "Some title".into()),
        ("author", "alice".into()),
        ("kind", "indicator".into()),
        ("visibility", "open".into()),
        ("published", "2024-03-01".into()),
        ("updated", "".into()),
        ("pine_version", "5".into()),
        ("licence_line", "-".into()),
        ("licence_class", "mpl2".into()),
        ("copied_utc", "2026-10-29T10:15Z".into()),
        ("how", "tv_source_view".into()),
        ("likes", "12".into()),
        ("boosts", "".into()),
        ("uses", "340".into()),
        ("notes", "free text".into()),
    ];
    for (k, v) in f {
        for c in cols.iter_mut() {
            if c.0 == *k {
                c.1 = v.to_string();
            }
        }
    }
    cols.into_iter()
        .map(|(_, v)| v)
        .collect::<Vec<_>>()
        .join("\t")
}

fn ledger(e: &Env, rows: &[String]) -> PathBuf {
    let p = e.vault.join("manual.tsv");
    let mut s = String::from(HEADER);
    for r in rows {
        s.push('\n');
        s.push_str(r);
    }
    s.push('\n');
    std::fs::write(&p, s).unwrap();
    p
}

fn put(e: &Env, file: &str, text: &str) {
    let p = e.intake.join(SESSION).join(file);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn run(e: &Env, tsv: &std::path::Path, check: bool) -> intake::Report {
    intake::import(
        &e.corpus,
        &Source {
            tsv,
            intake: &e.intake,
        },
        check,
    )
    .unwrap()
}

fn refusal(r: &intake::Report) -> String {
    r.rows
        .iter()
        .flat_map(|v| v.refused.clone())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn intake_accepts_a_clean_row_and_writes_one_complete_record() {
    let e = env("clean");
    let src = script("indicator(\"t\")\nplot(close)");
    put(&e, "001.pine", &src);
    let tsv = ledger(&e, &[row("001.pine", "AbC123", &[])]);
    let chk = run(&e, &tsv, true);
    assert_eq!(chk.accepted(), 1, "{}", refusal(&chk));
    assert!(!e.corpus.index_path().exists(), "--check writes nothing");
    let r = run(&e, &tsv, false);
    assert_eq!((r.accepted(), r.appended, r.written), (1, 1, 1));
    let idx = pine_corpus::index::load(&e.corpus.index_path()).unwrap();
    let rec = &idx.records[0];
    assert_eq!(rec.core.slug, "AbC123");
    assert_eq!(rec.core.status, "parsed");
    assert_eq!(rec.acq.as_deref(), Some("manual"));
    assert_eq!(rec.licence_class.as_deref(), Some("mpl2"));
    assert_eq!(rec.core.licence_override, None);
    assert_eq!(rec.core.http_status, None);
    assert_eq!(rec.core.raw_sha256, None);
    assert_eq!(rec.core.version_src, Some(5));
    assert_eq!(rec.core.uses, Some(340));
    assert_eq!(rec.core.boosts, None);
    let sha = pine_corpus::sha256(src.as_bytes());
    assert_eq!(rec.core.sha256, Some(sha));
    assert_eq!(
        std::fs::read_to_string(e.corpus.src_path(&sha)).unwrap(),
        src
    );
}

#[test]
fn intake_second_run_appends_nothing_and_is_byte_identical() {
    // I33.
    let e = env("idem");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    let tsv = ledger(&e, &[row("001.pine", "AbC123", &[])]);
    run(&e, &tsv, false);
    let a = std::fs::read(e.corpus.index_path()).unwrap();
    let r = run(&e, &tsv, false);
    assert_eq!(r.appended, 0);
    assert!(r.rows.iter().all(|v| !v.appends));
    assert_eq!(a, std::fs::read(e.corpus.index_path()).unwrap());
}

#[test]
fn intake_refuses_a_row_whose_visibility_is_not_open() {
    // I34.
    let e = env("vis");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    let tsv = ledger(&e, &[row("001.pine", "A1", &[("visibility", "protected")])]);
    let r = run(&e, &tsv, false);
    assert_eq!(r.refused(), 1);
    assert!(refusal(&r).contains("I34"));
    assert!(!e.corpus.index_path().exists());
}

#[test]
fn intake_refuses_a_missing_field() {
    let e = env("missing");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    let tsv = ledger(&e, &[row("001.pine", "A1", &[("title", "")])]);
    assert!(refusal(&run(&e, &tsv, true)).contains("title is empty"));
    // A ledger without a column is a file-level refusal.
    std::fs::write(&tsv, "session\tfile\n").unwrap();
    let err = intake::import(
        &e.corpus,
        &Source {
            tsv: &tsv,
            intake: &e.intake,
        },
        true,
    );
    assert!(err.is_err());
}

#[test]
fn intake_refuses_a_bom_or_a_cr_in_the_source() {
    let e = env("bomcr");
    put(
        &e,
        "001.pine",
        &format!("\u{feff}{}", script("indicator(\"t\")\nplot(close)")),
    );
    put(&e, "002.pine", &script("indicator(\"t\")\r\nplot(close)"));
    let tsv = ledger(
        &e,
        &[row("001.pine", "A1", &[]), row("002.pine", "A2", &[])],
    );
    let r = run(&e, &tsv, true);
    assert_eq!(r.refused(), 2);
    let why = refusal(&r);
    assert!(why.contains("BOM") && why.contains("CR"), "{why}");
}

#[test]
fn intake_refuses_a_version_mismatch() {
    let e = env("ver");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    let tsv = ledger(&e, &[row("001.pine", "A1", &[("pine_version", "6")])]);
    assert!(refusal(&run(&e, &tsv, true)).contains("pine_version"));
}

#[test]
fn intake_refuses_a_kind_the_declaration_contradicts() {
    let e = env("kind");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    let tsv = ledger(&e, &[row("001.pine", "A1", &[("kind", "strategy")])]);
    assert!(refusal(&run(&e, &tsv, true)).contains("declaration statement"));
}

#[test]
fn intake_refuses_a_licence_conflict() {
    let e = env("lic");
    put(
        &e,
        "001.pine",
        &script("// Licensed under CC BY-NC-SA 4.0\nindicator(\"t\")\nplot(close)"),
    );
    put(
        &e,
        "002.pine",
        &script("// MIT License\nindicator(\"t\")\nplot(close)"),
    );
    let tsv = ledger(
        &e,
        &[
            row("001.pine", "A1", &[]),
            row(
                "002.pine",
                "A2",
                &[
                    ("licence_line", "MIT License"),
                    ("licence_class", "copyleft"),
                ],
            ),
        ],
    );
    let r = run(&e, &tsv, true);
    assert_eq!(r.refused(), 2, "{}", refusal(&r));
    // The agreeing classification is accepted, and a refused class is always allowed.
    let tsv = ledger(
        &e,
        &[
            row(
                "001.pine",
                "A1",
                &[
                    ("licence_line", "Licensed under CC BY-NC-SA 4.0"),
                    ("licence_class", "nc"),
                ],
            ),
            row(
                "002.pine",
                "A2",
                &[
                    ("licence_line", "// MIT License"),
                    ("licence_class", "permissive"),
                ],
            ),
        ],
    );
    let r = run(&e, &tsv, true);
    assert_eq!(r.accepted(), 2, "{}", refusal(&r));
}

#[test]
fn intake_refuses_a_copy_time_outside_the_session() {
    // The "not in the future" check without a clock (I31): against the session log.
    let e = env("future");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    let tsv = ledger(
        &e,
        &[row(
            "001.pine",
            "A1",
            &[("copied_utc", "2026-10-29T12:00Z")],
        )],
    );
    assert!(refusal(&run(&e, &tsv, true)).contains("outside the session"));
    let tsv = ledger(
        &e,
        &[row("001.pine", "A1", &[("session", "h20261030T0900")])],
    );
    assert!(refusal(&run(&e, &tsv, true)).contains("no session log"));
}

#[test]
fn intake_refuses_an_intake_dir_inside_a_work_tree() {
    // I17 for the intake: a pasted source inside a git work tree.
    let e = env("intake-wt");
    let wt = Tmp::new("intake-wt-repo");
    std::fs::create_dir_all(wt.path().join(".git")).unwrap();
    let inside = wt.path().join("intake");
    std::fs::create_dir_all(inside.join(SESSION)).unwrap();
    let tsv = ledger(&e, &[row("001.pine", "A1", &[])]);
    let err = intake::import(
        &e.corpus,
        &Source {
            tsv: &tsv,
            intake: &inside,
        },
        true,
    );
    assert!(matches!(
        err,
        Err(pine_corpus::CorpusError::InsideWorkTree { .. })
    ));
}

#[test]
fn intake_never_edits_a_source() {
    // I35: a re-copy with other bytes is a new record and a new file; the old file stays.
    let e = env("noedit");
    let v1 = script("indicator(\"t\")\nplot(close)");
    put(&e, "001.pine", &v1);
    let tsv = ledger(&e, &[row("001.pine", "A1", &[])]);
    run(&e, &tsv, false);
    let v2 = script("indicator(\"t\")\nplot(open)");
    put(&e, "001.pine", &v2);
    let r = run(&e, &tsv, false);
    assert_eq!(r.appended, 1);
    let h1 = pine_corpus::sha256(v1.as_bytes());
    let h2 = pine_corpus::sha256(v2.as_bytes());
    assert_eq!(std::fs::read_to_string(e.corpus.src_path(&h1)).unwrap(), v1);
    assert_eq!(std::fs::read_to_string(e.corpus.src_path(&h2)).unwrap(), v2);
    let idx = pine_corpus::index::load(&e.corpus.index_path()).unwrap();
    assert_eq!(idx.records[0].core.sha256, Some(h2));
    assert_eq!(idx.records[0].rec, 2);
}

#[test]
fn intake_dates_follow_the_conservative_rule() {
    // I36.
    let d = parse_date("2024-03-01").unwrap();
    assert_eq!(d, 1_709_251_200_000);
    let copied = parse_utc_minute("2024-03-02T08:30Z").unwrap();
    assert_eq!(date_rule(d, copied), copied);
    let later = parse_utc_minute("2026-10-29T10:15Z").unwrap();
    assert_eq!(date_rule(d, later), d + 2 * 86_400_000);
    assert!(parse_date("2024-02-30").is_none());
    assert!(parse_date("2024-2-1").is_none());
    assert!(parse_utc_minute("2026-10-29T24:00Z").is_none());
    let e = env("dates");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    let tsv = ledger(
        &e,
        &[row(
            "001.pine",
            "A1",
            &[("published", "2026-10-28"), ("updated", "2026-10-29")],
        )],
    );
    run(&e, &tsv, false);
    let r = &pine_corpus::index::load(&e.corpus.index_path())
        .unwrap()
        .records[0];
    let (p, u, ret) = (
        r.core.published_ms.unwrap(),
        r.core.updated_ms.unwrap(),
        r.core.retrieved_ms.unwrap(),
    );
    assert!(p <= u && u <= ret, "{p} {u} {ret}");
    assert_eq!(u, ret, "a same-day update is capped at the copy time");
    let tsv = ledger(
        &e,
        &[row(
            "001.pine",
            "A1",
            &[("published", "2026-10-29"), ("updated", "2026-10-01")],
        )],
    );
    assert!(refusal(&run(&e, &tsv, true)).contains("updated precedes published"));
}

#[test]
fn intake_is_all_or_nothing() {
    let e = env("aon");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    put(&e, "002.pine", &script("indicator(\"u\")\nplot(open)"));
    let tsv = ledger(
        &e,
        &[
            row("001.pine", "A1", &[]),
            row("002.pine", "A2", &[("how", "crawler")]),
        ],
    );
    let r = run(&e, &tsv, false);
    assert_eq!((r.accepted(), r.refused(), r.appended), (1, 1, 0));
    assert!(!e.corpus.index_path().exists());
}

#[test]
fn intake_refuses_a_duplicate_slug_in_one_ledger() {
    let e = env("dup");
    put(&e, "001.pine", &script("indicator(\"t\")\nplot(close)"));
    let tsv = ledger(
        &e,
        &[row("001.pine", "A1", &[]), row("001.pine", "A1", &[])],
    );
    assert!(refusal(&run(&e, &tsv, true)).contains("already on line"));
}

#[test]
fn intake_warns_on_a_sixth_row_by_one_author() {
    let e = env("cap");
    let mut rows = Vec::new();
    for i in 0..6 {
        put(
            &e,
            &format!("{i:03}.pine"),
            &script(&format!("indicator(\"t\")\nplot(close * {i})")),
        );
        rows.push(row(&format!("{i:03}.pine"), &format!("S{i}"), &[]));
    }
    let tsv = ledger(&e, &rows);
    let r = run(&e, &tsv, true);
    assert_eq!(r.accepted(), 6);
    assert_eq!(r.rows.iter().filter(|v| !v.warnings.is_empty()).count(), 1);
}

#[test]
fn intake_slug_is_the_script_segment_up_to_its_first_dash() {
    assert_eq!(
        slug_of("https://example.invalid/script/AbC123-the-title/").as_deref(),
        Some("AbC123")
    );
    assert_eq!(
        slug_of("https://h.example/x/script/Q9/").as_deref(),
        Some("Q9")
    );
    assert_eq!(slug_of("http://example.invalid/script/A-b/"), None);
    assert_eq!(slug_of("https://example.invalid/scripts/A-b/"), None);
    assert_eq!(slug_of("https://example.invalid/script/-x/"), None);
}

#[test]
fn intake_session_ids_and_logs() {
    assert!(intake::session_ok("h20261029T1000"));
    assert!(!intake::session_ok("h2026102T1000"));
    let (s, e) = intake::session_window("**start**: 2026-10-29T10:00Z\nend: 2026-10-29T11:00Z\n");
    assert_eq!(s, parse_utc_minute("2026-10-29T10:00Z"));
    assert_eq!(e, parse_utc_minute("2026-10-29T11:00Z"));
}

#[test]
fn intake_ledger_with_a_bom_or_cr_is_refused() {
    let e = env("ledger");
    let tsv = e.vault.join("manual.tsv");
    std::fs::write(&tsv, format!("\u{feff}{HEADER}\n")).unwrap();
    assert!(intake::import(
        &e.corpus,
        &Source {
            tsv: &tsv,
            intake: &e.intake
        },
        true
    )
    .is_err());
    std::fs::write(&tsv, format!("{HEADER}\r\n")).unwrap();
    assert!(intake::import(
        &e.corpus,
        &Source {
            tsv: &tsv,
            intake: &e.intake
        },
        true
    )
    .is_err());
}

fn lib_ledger(e: &Env, rows: &[String]) -> PathBuf {
    let p = e.vault.join("libs.tsv");
    let mut s = String::from(LIB_HEADER);
    for r in rows {
        s.push('\n');
        s.push_str(r);
    }
    s.push('\n');
    std::fs::write(&p, s).unwrap();
    p
}

fn lib_row(user: &str, lib: &str, ver: u32, file: &str, outcome: &str) -> String {
    [
        SESSION.to_string(),
        file.to_string(),
        format!("https://example.invalid/script/L{ver}-lib/"),
        lib.to_string(),
        user.to_string(),
        "library".into(),
        "open".into(),
        "2024-01-01".into(),
        "".into(),
        "5".into(),
        "-".into(),
        "mpl2".into(),
        "2026-10-29T10:20Z".into(),
        "tv_source_view".into(),
        "".into(),
        user.to_string(),
        lib.to_string(),
        ver.to_string(),
        outcome.to_string(),
    ]
    .join("\t")
}

#[test]
fn intake_lib_writes_the_cache_the_lock_and_libs_jsonl() {
    // I12: the lock is rewritten whole from the cached files and verifies by recomputation.
    let e = env("lib");
    let lib = script("library(\"Utils\")\nexport double(float x) =>\n    x * 2");
    put(&e, "lib/alice/Utils/3.pine", &lib);
    let tsv = lib_ledger(
        &e,
        &[
            lib_row("alice", "Utils", 3, "lib/alice/Utils/3.pine", "ok"),
            lib_row("carol", "Gone", 2, "-", "lib_missing"),
        ],
    );
    let r = intake::import_lib(
        &e.corpus,
        &Source {
            tsv: &tsv,
            intake: &e.intake,
        },
        false,
    )
    .unwrap();
    assert_eq!((r.accepted(), r.appended), (2, 2), "{}", refusal(&r));
    assert_eq!(pine_corpus::libs::verify_lock(&e.corpus), Ok(1));
    let rows = pine_corpus::index::load_libs(&e.corpus.libs_path()).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].outcome, "ok");
    assert_eq!(rows[0].licence_class.as_deref(), Some("mpl2"));
    assert_eq!(rows[1].outcome, "lib_missing");
    // Idempotent (I33).
    let a = std::fs::read(e.corpus.libs_path()).unwrap();
    let r = intake::import_lib(
        &e.corpus,
        &Source {
            tsv: &tsv,
            intake: &e.intake,
        },
        false,
    )
    .unwrap();
    assert_eq!(r.appended, 0);
    assert_eq!(a, std::fs::read(e.corpus.libs_path()).unwrap());
    // A pinned version never changes: other bytes for the same key are refused.
    put(
        &e,
        "lib/alice/Utils/3.pine",
        &script("library(\"Utils\")\nexport double(float x) =>\n    x * 3"),
    );
    let r = intake::import_lib(
        &e.corpus,
        &Source {
            tsv: &tsv,
            intake: &e.intake,
        },
        true,
    )
    .unwrap();
    assert!(refusal(&r).contains("already cached"));
}

#[test]
fn intake_lib_refuses_a_non_library_and_a_wrong_author() {
    let e = env("libbad");
    put(
        &e,
        "lib/alice/Utils/3.pine",
        &script("indicator(\"t\")\nplot(close)"),
    );
    let mut bad_author = lib_row("alice", "Utils", 3, "lib/alice/Utils/3.pine", "ok");
    bad_author = bad_author.replacen("\talice\tlibrary", "\tmallory\tlibrary", 1);
    let tsv = lib_ledger(&e, &[bad_author]);
    let r = intake::import_lib(
        &e.corpus,
        &Source {
            tsv: &tsv,
            intake: &e.intake,
        },
        true,
    )
    .unwrap();
    let why = refusal(&r);
    assert!(
        why.contains("publishing user") && why.contains("declaration statement"),
        "{why}"
    );
}

#[test]
fn licence_patterns_follow_the_spec_regexes() {
    assert!(licence::licence_match(
        "This work is licensed under a Creative Commons"
    ));
    assert!(licence::licence_match("CC BY-NC-SA 4.0"));
    assert!(licence::licence_match("GPL-3.0"));
    assert!(!licence::licence_match("LGPL")); // \bgpl needs a boundary before `g`
    assert!(licence::licence_match("All Rights Reserved"));
    assert!(!licence::licence_match("non commercial"));
    assert!(licence::licence_match("noncommercial use"));
    assert!(!licence::licence_match("© alice"));
    assert!(licence::mpl_match("Mozilla Public License 2.0"));
    assert!(licence::mpl_match("MPL-2.0"));
    assert!(licence::mpl_match("mpl 2"));
    assert!(!licence::mpl_match("mpl20x"));
    assert_eq!(
        licence::source_licence_line("//@version=5\n// © alice\n// This source code is subject to the terms of the Mozilla Public License 2.0\n// Licensed MIT\n"),
        Some("Licensed MIT".to_string())
    );
}

#[test]
fn licence_class_table_and_admission() {
    assert_eq!(licence::ADMITTED, ["mpl2", "permissive", "copyleft"]);
    for c in ["nc", "nd", "arr", "custom", "unknown"] {
        assert!(!licence::admitted(c));
        assert!(licence::CLASSES.contains(&c));
    }
    assert_eq!(licence::implied_class("CC BY-NC 4.0"), Some("nc"));
    assert_eq!(licence::implied_class("CC BY-ND"), Some("nd"));
    assert_eq!(licence::implied_class("All rights reserved"), Some("arr"));
    assert_eq!(licence::implied_class("GNU GPL v3"), Some("copyleft"));
    assert_eq!(licence::implied_class("MIT License"), Some("permissive"));
    assert_eq!(
        licence::implied_class("Mozilla Public License 2.0"),
        Some("mpl2")
    );
    // OH-6(c): a republication restriction alone leaves the default terms.
    assert_eq!(licence::implied_class("Do not republish"), Some("mpl2"));
    assert!(licence::check("//@version=5\n", "Do not republish", "mpl2").is_ok());
    assert!(licence::check("//@version=5\n", "-", "mpl2")
        .unwrap()
        .is_none());
    assert!(licence::check("//@version=5\n", "-", "permissive").is_err());
    assert!(licence::check("//@version=5\n", "personal use only", "custom").is_ok());
    assert!(licence::check("//@version=5\n", "personal use only", "mpl2").is_err());
    assert!(licence::check("//@version=5\n", "-", "bogus").is_err());
}
