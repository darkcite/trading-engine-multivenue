// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Durable state-file writes, and where a member's state file lives.
//!
//! `write_atomic` MOVED to `core-io` in E4: `exec-hyperliquid` needs the
//! same discipline and sits below this crate, and two implementations of a
//! function whose entire point is one easily-forgotten `sync_all` is one
//! more than can be audited. Re-exported here so every existing caller and
//! every existing path is unchanged.

use std::path::{Path, PathBuf};

pub use core_io::state_file::write_atomic;

/// F22: where a member's OWN state file lives (VRP's law, shared by the
/// slot-7 book since HC11b).
///
/// * `Some(p)` — an explicit `--<member>-state`: that file, whatever else.
/// * `None` with an EXPLICIT artifact path — `file_name` beside that
///   artifact. A smoke boot on its own artifact must not read and rewrite
///   the standing engine's state, and "beside the artifact" is the rule
///   that needs no second flag to be safe.
/// * `None` with the default artifact — `default()`.
///
/// # Errors
///
/// `default()`'s (the home directory could not be resolved).
pub fn resolve_state_path(
    state_path: Option<&Path>,
    artifact: &Path,
    artifact_explicit: bool,
    file_name: &str,
    default: impl FnOnce() -> Result<String, String>,
) -> Result<PathBuf, String> {
    if let Some(p) = state_path {
        return Ok(p.to_path_buf());
    }
    if artifact_explicit {
        if let Some(dir) = artifact.parent() {
            return Ok(dir.join(file_name));
        }
    }
    Ok(PathBuf::from(default()?))
}

/// Read a member's state file. Only a file that is NOT THERE is `Ok(None)`
/// — a first boot has no history, which is normal and not an error.
///
/// # Errors
///
/// Anything else that stops the read — a directory it may not search, a
/// path that is not a file, bytes that are not UTF-8: a boot must not
/// pretend it had no history (`Path::exists` would have read every such
/// error as absence — HC11b review).
pub fn read_state(kind: &str, path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{kind}: {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absent is a first boot; present is read; anything else refuses.
    #[test]
    fn only_a_missing_file_is_no_history() {
        let d = std::env::temp_dir().join(format!("state-file-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(read_state("x", &d.join("absent.tsv")), Ok(None));
        std::fs::write(d.join("book.tsv"), "V\t1\n").unwrap();
        assert_eq!(read_state("x", &d.join("book.tsv")), Ok(Some("V\t1\n".to_owned())));
        std::fs::create_dir_all(d.join("a-dir.tsv")).unwrap();
        assert!(read_state("x", &d.join("a-dir.tsv")).unwrap_err().starts_with("x: "));
        std::fs::write(d.join("binary.tsv"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(read_state("x", &d.join("binary.tsv")).is_err(), "not UTF-8");
        let _ = std::fs::remove_dir_all(&d);
    }
}
