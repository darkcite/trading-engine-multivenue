// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Build script (RG6): record the checkout's git commit into
//! `MULTIVENUE_GIT_SHA` so `/state` `boot.git_sha` names the build.
//! Git is NOT a build dependency — any failure yields `unknown`. The
//! script re-runs only when `HEAD` (or the branch ref it points to)
//! moves, so an ordinary source edit never re-runs it; the binary's
//! own mtime (also in `/state`) is the relink tell.
//!
//! **PMJ J16 — slot 11's gate lines, compiled in.** The engine arms a live
//! slot 11 only when the tree it was BUILT from has, in
//! `docs/risk-policy.md`, a line that BEGINS with `PMJ-DUST-GATE: ruled
//! YYYY-MM-DD` (`PMJ_DUST_GATE_LINE=1`), and a frame beyond the dust frame
//! only with a line beginning `PMJ-LIVE-GATE: ruled YYYY-MM-DD` too
//! (`PMJ_LIVE_GATE_LINE=1`; the pilot's, plan-J8). The grammar is
//! `src/gate_line.rs`, `include!`d here and unit-tested in the library;
//! a line inside a fenced code block, or quoting the word mid-line, opens
//! nothing. **A flag is set only when the line is in the working-tree file
//! AND in the COMMITTED one** (`git --no-optional-locks -C <root> show
//! HEAD:docs/risk-policy.md`): an uncommitted gate line opens nothing, and
//! any failure (no git, no repo, no such file at HEAD) is 0. The script
//! re-runs when the file, `.git/HEAD` or the ref it points to changes, so
//! the flags describe the tree and the commit the binary was built from
//! (`cli::exec_boot::PMJ_GATES_BUILT`). **Revoking a gate** is
//! `scripts/pmj-flip.sh paper` and a rebuild from a tree without the line:
//! a binary keeps the flags it was built with.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let root = workspace_root();
    let sha = git_head_sha(&root).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MULTIVENUE_GIT_SHA={sha}");
    // PMJ J16: the gate lines (module doc). Declared even when missing:
    // a missing policy re-runs the script every build and stays 0.
    let (dust, live) = gate_lines(&root);
    println!("cargo:rustc-env=PMJ_DUST_GATE_LINE={}", u8::from(dust));
    println!("cargo:rustc-env=PMJ_LIVE_GATE_LINE={}", u8::from(live));
    println!("cargo:rerun-if-changed=../../docs/risk-policy.md");
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

// PMJ J16: the gate grammar, shared with the library (`cli::gate_line`).
include!("src/gate_line.rs");

/// `(dust, live)`: each gate line present in the working-tree
/// `docs/risk-policy.md` AND in the committed one (module doc); a missing
/// or unreadable file, or any git failure, is neither.
fn gate_lines(root: &Path) -> (bool, bool) {
    let Ok(text) = std::fs::read_to_string(root.join("docs").join("risk-policy.md")) else {
        return (false, false);
    };
    let tree = gate_lines_in(&text);
    if !tree.0 && !tree.1 {
        return (false, false);
    }
    let Some(head) = git_show_policy(root) else {
        return (false, false);
    };
    let committed = gate_lines_in(&head);
    (tree.0 && committed.0, tree.1 && committed.1)
}

/// `git show HEAD:docs/risk-policy.md` (read-only, no optional locks).
fn git_show_policy(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(["show", "HEAD:docs/risk-policy.md"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
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
