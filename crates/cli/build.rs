// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Build script (RG6): record the checkout's git commit into
//! `MULTIVENUE_GIT_SHA` so `/state` `boot.git_sha` names the build.
//! Git is NOT a build dependency — any failure yields `unknown`. The
//! script re-runs only when `HEAD` (or the branch ref it points to)
//! moves, so an ordinary source edit never re-runs it; the binary's
//! own mtime (also in `/state`) is the relink tell.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let root = workspace_root();
    let sha = git_head_sha(&root).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MULTIVENUE_GIT_SHA={sha}");
    // Re-run when the commit moves. Only existing paths are declared:
    // a declared-but-missing path makes cargo re-run every build.
    let head = root.join(".git").join("HEAD");
    if head.is_file() {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Some(r) = symbolic_ref(&head) {
            let target = root.join(".git").join(r);
            if target.is_file() {
                println!("cargo:rerun-if-changed={}", target.display());
            }
        }
    }
}

fn workspace_root() -> PathBuf {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    manifest
        .and_then(|m| m.parent().and_then(Path::parent).map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn git_head_sha(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(s.to_string())
}

/// `ref: refs/heads/main` → `Some("refs/heads/main")`; a detached HEAD
/// holds a bare sha → `None`.
fn symbolic_ref(head: &Path) -> Option<String> {
    let text = std::fs::read_to_string(head).ok()?;
    let r = text.strip_prefix("ref: ")?.trim();
    if r.is_empty() {
        None
    } else {
        Some(r.to_string())
    }
}
