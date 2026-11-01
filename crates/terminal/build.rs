// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! With the `web-bundle` feature: embed the terminal UI bundle, refusing the
//! build unless every file matches its SHA-256 in `dist/MANIFEST`. Without
//! the feature: an empty embed (the server loads `--bundle DIR` instead).

use std::fmt::Write as _;
use std::path::PathBuf;

const FILES: [&str; 5] = ["index.html", "loader.js", "term.css", "term_ui.js", "term_ui_bg.wasm"];

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let target = out.join("bundle_embed.rs");
    println!("cargo:rerun-if-env-changed=TERM_WEB_DIST");
    if std::env::var_os("CARGO_FEATURE_WEB_BUNDLE").is_none() {
        std::fs::write(&target, "pub const EMBEDDED: &[(&str, &[u8])] = &[];\n").expect("write embed");
        return;
    }
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let dist = std::env::var_os("TERM_WEB_DIST").map(PathBuf::from).unwrap_or_else(|| manifest_dir.join("../../terminal-ui/dist"));
    let manifest = std::fs::read_to_string(dist.join("MANIFEST")).unwrap_or_else(|e| panic!("web-bundle: {}/MANIFEST: {e} — run terminal-ui/scripts/terminal-ui-build.sh", dist.display()));
    println!("cargo:rerun-if-changed={}", dist.join("MANIFEST").display());
    let mut src = String::from("pub const EMBEDDED: &[(&str, &[u8])] = &[\n");
    for name in FILES {
        let path = dist.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let body = std::fs::read(&path).unwrap_or_else(|e| panic!("web-bundle: {}: {e}", path.display()));
        let mut got = String::new();
        for b in core_crypto::sha256(&body) {
            let _ = write!(got, "{b:02x}");
        }
        let want = manifest.lines().find_map(|l| l.split_once("  ").filter(|(_, n)| *n == name).map(|(h, _)| h.to_owned())).unwrap_or_else(|| panic!("web-bundle: {name} not in MANIFEST"));
        assert_eq!(got, want, "web-bundle: {name} does not match dist/MANIFEST — rebuild the bundle");
        let abs = std::fs::canonicalize(&path).expect("canonical path");
        let _ = writeln!(src, "    ({name:?}, include_bytes!({:?})),", abs.display().to_string());
    }
    src.push_str("];\n");
    std::fs::write(&target, src).expect("write embed");
}
