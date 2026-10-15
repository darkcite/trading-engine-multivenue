// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.10 — pass 1, the import graph, and the library cache (§4.2).

mod common;

use common::{codes, compile_with_libs, diags, v5, NOT_IMPLEMENTED};
use pine_ir::{fnv1a64_continue, FNV1A64_OFFSET};
use pine_sema::{LibCache, LibKey};

fn key(user: &str, lib: &str, ver: u16) -> LibKey {
    LibKey {
        user: user.to_owned(),
        lib: lib.to_owned(),
        ver,
    }
}

fn lib_src(name: &str, imports: &str) -> String {
    format!("//@version=5\nlibrary(\"{name}\")\n{imports}export f(float x) => x\n")
}

fn root(imports: &str) -> String {
    format!("//@version=5\nindicator(\"t\")\n{imports}plot(close)\n")
}

/// A stand-in `Sha256Fn` (the cache only compares digests; pine-sema has no core-crypto edge).
fn fake_sha(b: &[u8]) -> [u8; 32] {
    let mut d = [0u8; 32];
    d[..8].copy_from_slice(&fnv1a64_continue(FNV1A64_OFFSET, b).to_le_bytes());
    d
}

fn hex(d: &[u8; 32]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh directory under cargo's per-target temp dir.
fn tmp_dir(name: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("pine-sema-imports")
        .join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn write_lib(root: &std::path::Path, k: &LibKey, text: &str) {
    let d = root.join(&k.user).join(&k.lib);
    std::fs::create_dir_all(&d).expect("lib dir");
    std::fs::write(d.join(format!("{}.pine", k.ver)), text).expect("lib file");
}

#[test]
fn imports_default_alias_is_the_lib_name() {
    // `import u/a/1` and `import v/a/1` both default to alias `a`: the second is a duplicate.
    let mut libs = LibCache::in_memory(vec![
        (key("u", "a", 1), lib_src("a", "")),
        (key("v", "a", 1), lib_src("a", "")),
    ]);
    let r = compile_with_libs(&root("import u/a/1\nimport v/a/1\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![3041]);
    let r = compile_with_libs(&root("import u/a/1\nimport v/a/1 as b\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
}

#[test]
fn imports_duplicate_alias_is_refused() {
    let mut libs = LibCache::in_memory(vec![
        (key("u", "a", 1), lib_src("a", "")),
        (key("u", "b", 1), lib_src("b", "")),
    ]);
    let r = compile_with_libs(
        &root("import u/a/1 as m\nimport u/b/1 as m\n"),
        &v5(),
        &mut libs,
    );
    assert_eq!(codes(&r), vec![3041]);
}

#[test]
fn imports_alias_equal_to_a_namespace_is_reserved() {
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), lib_src("l", ""))]);
    let r = compile_with_libs(&root("import u/l/1 as ta\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![3004]);
}

#[test]
fn imports_missing_library_is_refused() {
    let dir = tmp_dir("missing");
    let mut libs = LibCache::open(dir.clone(), fake_sha).expect("no lock is fine");
    let ds = diags(compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs));
    assert_eq!(ds[0].code.0, 3034);
    let path = dir.join("u").join("l").join("1.pine");
    assert!(
        ds[0].msg.contains(&path.display().to_string()),
        "{}",
        ds[0].msg
    );
    // the span is the import statement
    assert_eq!(ds[0].span.file.0, 0);
    assert!(ds[0].span.lo > 0);
}

#[test]
fn imports_path_segments_cannot_escape_the_root() {
    for (u, l) in [("..", "x"), (".", "x"), ("u", ".."), ("a\\b", "x")] {
        let mut libs = LibCache::in_memory(vec![(key(u, l, 1), lib_src("x", ""))]);
        let src = root(&format!("import {u}/{l}/1 as m\n"));
        let mut syms = pine_ir::SymTable::new();
        let Ok(ast) = pine_lang::parse(&src, pine_ir::FileId::ROOT, &mut syms) else {
            continue; // the parser refuses the spelling itself: nothing reaches the cache
        };
        let r = pine_sema::compile(&ast, syms, &v5(), &mut libs);
        let ds = diags(r);
        assert_eq!(ds[0].code.0, 3034, "{u}/{l}");
        assert!(ds[0].msg.contains("invalid library path segment"));
        assert!(libs.loaded().is_empty());
    }
}

#[test]
fn imports_lock_mismatch_is_refused() {
    let dir = tmp_dir("mismatch");
    let k = key("u", "l", 1);
    let text = lib_src("l", "");
    write_lib(&dir, &k, &text);
    let pin = [0xab; 32];
    std::fs::write(
        dir.join("lib.lock"),
        format!("\"u/l/1\" = \"{}\"\n", hex(&pin)),
    )
    .unwrap();
    let mut libs = LibCache::open(dir, fake_sha).expect("lock parses");
    let ds = diags(compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs));
    assert_eq!(ds[0].code.0, 3037);
    assert!(ds[0].msg.contains(&hex(&pin)));
    assert!(ds[0].msg.contains(&hex(&fake_sha(text.as_bytes()))));
}

#[test]
fn imports_lock_match_is_pinned() {
    let dir = tmp_dir("match");
    let k = key("u", "l", 1);
    let text = lib_src("l", "");
    write_lib(&dir, &k, &text);
    let lock = format!(
        "# pins\n\n\"u/l/1\" = \"{}\"\r\n",
        hex(&fake_sha(text.as_bytes()))
    );
    std::fs::write(dir.join("lib.lock"), lock).unwrap();
    let mut libs = LibCache::open(dir, fake_sha).expect("lock parses");
    let r = compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
    assert_eq!(libs.loaded().len(), 1);
    assert!(libs.loaded()[0].pinned);
    assert_eq!(libs.loaded()[0].text(), text);
}

#[test]
fn imports_malformed_lock_is_refused() {
    let cases: [(&str, &str); 5] = [
        ("\"u/l/1\" = \"zz\"\n", "line 1"),
        ("# c\n\"u/l/0\" = \"%\"\n", "line 2"),
        ("\n\n\"u/l\" = \"%\"\n", "line 3"),
        ("\"u/l/1\" = \"%\"\n\"u/l/1\" = \"%\"\n", "line 2"),
        ("\"u/l/1\" = \"%\" x\n", "line 1"),
    ];
    let good = hex(&[7; 32]);
    for (i, (text, line)) in cases.iter().enumerate() {
        let dir = tmp_dir(&format!("malformed{i}"));
        std::fs::write(dir.join("lib.lock"), text.replace('%', &good)).unwrap();
        let d = LibCache::open(dir, fake_sha).expect_err("malformed");
        assert_eq!(d.code.0, 3037, "{text}");
        assert!(d.msg.contains(line), "{text}: {}", d.msg);
    }
    let dir = tmp_dir("malformed-utf8");
    std::fs::write(dir.join("lib.lock"), b"# ok\n\xff\n").unwrap();
    let d = LibCache::open(dir, fake_sha).expect_err("not utf-8");
    assert!(d.msg.contains("line 2"));
}

#[test]
fn imports_unpinned_library_loads_and_is_listed() {
    let dir = tmp_dir("unpinned");
    let k = key("u", "l", 1);
    let text = lib_src("l", "");
    write_lib(&dir, &k, &text);
    let mut libs = LibCache::open(dir, fake_sha).expect("no lock");
    let r = compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
    assert_eq!(libs.loaded().len(), 1);
    assert!(!libs.loaded()[0].pinned);
    assert_eq!(libs.loaded()[0].key, k);
    assert_eq!(libs.loaded()[0].sha256, fake_sha(text.as_bytes()));
}

#[test]
fn imports_transitive_walk_loads_grandchildren() {
    let mut libs = LibCache::in_memory(vec![
        (key("u", "a", 1), lib_src("a", "import u/b/1\n")),
        (key("u", "b", 1), lib_src("b", "import u/c/1\n")),
        (key("u", "c", 1), lib_src("c", "")),
    ]);
    let r = compile_with_libs(&root("import u/a/1\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
    let got: Vec<&str> = libs.loaded().iter().map(|s| s.key.lib.as_str()).collect();
    assert_eq!(got, ["a", "b", "c"]);
    assert!(libs
        .loaded()
        .iter()
        .all(|s| !s.pinned && s.sha256 == [0; 32]));
}

#[test]
fn imports_file_ids_are_dfs_preorder() {
    // root imports a (→ c, broken) then b (broken): DFS pre-order is a=1, c=2, b=3, and each
    // library's parse error names its FileId.
    let broken = "//@version=5\nlibrary(\"x\")\nexport f( => \n";
    let mut libs = LibCache::in_memory(vec![
        (key("u", "a", 1), lib_src("a", "import u/c/1\n")),
        (key("u", "b", 1), broken.to_owned()),
        (key("u", "c", 1), broken.to_owned()),
    ]);
    let ds = diags(compile_with_libs(
        &root("import u/a/1\nimport u/b/1\n"),
        &v5(),
        &mut libs,
    ));
    let files: Vec<u16> = ds.iter().map(|d| d.span.file.0).collect();
    assert!(files.contains(&2) && files.contains(&3), "{files:?} {ds:?}");
    assert!(!files.contains(&1));
    let loaded: Vec<&str> = libs.loaded().iter().map(|s| s.key.lib.as_str()).collect();
    assert_eq!(loaded, ["a", "c", "b"]);
}

#[test]
fn imports_depth_above_eight_is_refused() {
    let mut entries = Vec::new();
    for i in 1..=9u16 {
        let next = if i < 9 {
            format!("import u/l{}/1\n", i + 1)
        } else {
            String::new()
        };
        entries.push((
            key("u", &format!("l{i}"), 1),
            lib_src(&format!("l{i}"), &next),
        ));
    }
    let mut libs = LibCache::in_memory(entries.clone());
    let ds = diags(compile_with_libs(
        &root("import u/l1/1\n"),
        &v5(),
        &mut libs,
    ));
    assert_eq!(ds.iter().map(|d| d.code.0).collect::<Vec<_>>(), vec![3036]);
    assert_eq!(ds[0].span.file.0, 8, "raised at l8's import of l9");
    // eight deep is fine
    entries[7].1 = lib_src("l8", "");
    let mut libs = LibCache::in_memory(entries);
    let r = compile_with_libs(&root("import u/l1/1\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
}

#[test]
fn imports_cycle_is_refused_with_the_path() {
    let mut libs = LibCache::in_memory(vec![
        (key("a", "x", 1), lib_src("x", "import b/y/2\n")),
        (key("b", "y", 2), lib_src("y", "import a/x/1\n")),
    ]);
    let ds = diags(compile_with_libs(&root("import a/x/1\n"), &v5(), &mut libs));
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].code.0, 3035);
    assert_eq!(ds[0].notes[0].1, "a/x/1 → b/y/2 → a/x/1");
}

#[test]
fn imports_two_versions_of_one_lib_are_not_a_cycle() {
    let mut libs = LibCache::in_memory(vec![
        (key("u", "l", 2), lib_src("l", "import u/l/1 as old\n")),
        (key("u", "l", 1), lib_src("l", "")),
    ]);
    let r = compile_with_libs(&root("import u/l/2\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
    assert_eq!(libs.loaded().len(), 2);
}

#[test]
fn imports_library_in_other_dialect_is_refused() {
    let v6 = "//@version=6\nlibrary(\"l\")\nexport f(float x) => x\n".to_owned();
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), v6)]);
    let src = root("import u/l/1\n");
    let ds = diags(compile_with_libs(&src, &v5(), &mut libs));
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].code.0, 3002);
    assert_eq!(ds[0].span.file.0, 0);
    assert_eq!(
        &src[ds[0].span.lo as usize..ds[0].span.lo as usize + 6],
        "import"
    );
}

#[test]
fn imports_v4_library_is_refused() {
    let v4 = "//@version=4\nstudy(\"l\")\n".to_owned();
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), v4)]);
    let ds = diags(compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs));
    let at: Vec<(u16, u16)> = ds.iter().map(|d| (d.span.file.0, d.code.0)).collect();
    assert_eq!(at, vec![(0, 3001), (1, 3001)]);
}

#[test]
fn imports_library_parse_errors_are_returned() {
    let broken = "//@version=5\nlibrary(\"x\")\nexport f( => \n".to_owned();
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), broken)]);
    let ds = diags(compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs));
    assert!(!ds.is_empty());
    assert!(
        ds.iter()
            .all(|d| d.span.file.0 == 1 && (1000..3000).contains(&d.code.0)),
        "{ds:?}"
    );
}

#[test]
fn refusal_library_diagnostics_name_the_library_file() {
    let mut libs = LibCache::in_memory(vec![(key("u", "a", 1), lib_src("a", "import u/zz/1\n"))]);
    let ds = diags(compile_with_libs(&root("import u/a/1\n"), &v5(), &mut libs));
    assert_eq!(ds.len(), 1);
    assert_eq!((ds[0].code.0, ds[0].span.file.0), (3034, 1));
}

#[test]
fn imports_library_is_reparsed_per_compilation() {
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), lib_src("l", ""))]);
    for _ in 0..2 {
        let r = compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs);
        assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
    }
    assert_eq!(
        libs.loaded().len(),
        1,
        "the source is read once per cache life"
    );
}

#[test]
fn libcache_empty_refuses_every_import() {
    let mut libs = LibCache::empty();
    let ds = diags(compile_with_libs(
        &root("import u/a/1\nimport u/b/1\n"),
        &v5(),
        &mut libs,
    ));
    assert_eq!(
        ds.iter().map(|d| d.code.0).collect::<Vec<_>>(),
        vec![3034, 3034]
    );
    assert_eq!(LibCache::default().loaded().len(), 0);
}

#[test]
fn libcache_in_memory_and_dir_agree() {
    let dir = tmp_dir("agree");
    let k = key("u", "l", 1);
    let text = lib_src("l", "");
    write_lib(&dir, &k, &text);
    let mut a = LibCache::open(dir, fake_sha).expect("no lock");
    let mut b = LibCache::in_memory(vec![(k, text)]);
    let src = root("import u/l/1\n");
    assert_eq!(
        codes(&compile_with_libs(&src, &v5(), &mut a)),
        codes(&compile_with_libs(&src, &v5(), &mut b))
    );
    assert_eq!(a.loaded()[0].text(), b.loaded()[0].text());
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]
    #[test]
    fn libcache_lock_reader_never_panics(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
        let dir = tmp_dir("prop");
        std::fs::write(dir.join("lib.lock"), &bytes).unwrap();
        if let Err(d) = LibCache::open(dir, fake_sha) {
            proptest::prop_assert_eq!(d.code.0, 3037);
        }
    }
}

#[test]
fn imports_non_exported_item_is_refused() {
    let lib = "//@version=5\nlibrary(\"l\")\nhidden(float x) => x\nexport shown(float x) => x\n";
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), lib.to_owned())]);
    let r = compile_with_libs(
        &root("import u/l/1\nx = l.hidden(close)\n"),
        &v5(),
        &mut libs,
    );
    assert_eq!(codes(&r), vec![3038]);
    let r = compile_with_libs(
        &root("import u/l/1\nx = l.nonesuch(close)\n"),
        &v5(),
        &mut libs,
    );
    assert_eq!(codes(&r), vec![3003]);
    let r = compile_with_libs(
        &root("import u/l/1\nx = l.shown(close)\n"),
        &v5(),
        &mut libs,
    );
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
}

#[test]
fn imports_library_types_resolve_through_the_alias() {
    let lib = "//@version=5\nlibrary(\"l\")\nexport type P\n    float v = 0.0\ntype H\n    float v = 0.0\nexport f(float x) => x\n";
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), lib.to_owned())]);
    let r = compile_with_libs(&root("import u/l/1\nl.P p = l.P.new()\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
    let r = compile_with_libs(&root("import u/l/1\nl.H h = na\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![3038]);
}

#[test]
fn resolve_library_function_sees_only_const_globals() {
    let lib = |body: &str| format!("//@version=5\nlibrary(\"l\")\n{body}");
    let ok = lib("const float K = 2.0\nM = 3\nexport f(float x) => x * K + M\n");
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), ok)]);
    let r = compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs);
    assert_eq!(codes(&r), vec![NOT_IMPLEMENTED]);
    let bad = lib("G = close\nexport f(float x) => x + G\n");
    let mut libs = LibCache::in_memory(vec![(key("u", "l", 1), bad)]);
    let ds = diags(compile_with_libs(&root("import u/l/1\n"), &v5(), &mut libs));
    assert_eq!((ds[0].code.0, ds[0].span.file.0), (3003, 1));
    assert_eq!(
        ds[0].notes[0].1,
        "library functions see only `const` globals"
    );
}
