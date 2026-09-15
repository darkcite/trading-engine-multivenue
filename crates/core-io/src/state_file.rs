// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The one durable write for every engine-written state file.
//!
//! Members persist their own state — the VRP campaign, the xsd table
//! hash, and since E4 the Hyperliquid request budget — and every one
//! of them carried the same defect once: a temp file and a rename with
//! **no `sync_all`**. On APFS a crash between the write and the rename
//! can leave a zero-length file, and a zero-length state file boots as
//! "first boot" — silently, with no campaign to settle and no window
//! to arm the halt.
//!
//! `rename` is atomic with respect to READERS on the same filesystem;
//! it is NOT a durability barrier. The data has to reach the disk
//! before the name does, or the name can arrive first.
//!
//! It lives HERE, in `core-io`, rather than in `cli`, because the
//! crates that need it now sit below `cli` — and a second copy of a
//! function whose entire point is one easily-forgotten `sync_all` is
//! how that bug comes back.
//!
//! BOOT/OFFLINE DOCTRINE: this runs on the 5 s observability cadence
//! and at shutdown, never on the tick path. Allocation is fine here.

use std::io::Write as _;
use std::path::Path;

/// Write `text` to `path` durably: a temp file beside it, `sync_all`,
/// then `rename`.
///
/// The temp file is overwritten if one is already there — a previous
/// crash between `write` and `rename` leaves one behind, and refusing to
/// write because of it would turn one lost campaign into every future
/// campaign.
///
/// # Errors
/// The temp write, the fsync or the rename failed; the message names the
/// path.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    // APPEND `.tmp` rather than REPLACE the extension. The original
    // hard-coded `with_extension("tsv.tmp")`, which is right for
    // `*.tsv` and silently wrong for anything else — E4's
    // `exec-budget.state` would have staged through
    // `exec-budget.tsv.tmp`, a name belonging to no file here.
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    {
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(text.as_bytes())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        // The whole point: the bytes reach the disk before the name does.
        f.sync_all()
            .map_err(|e| format!("{}: fsync: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_of(p: &Path) -> std::path::PathBuf {
        let mut t = p.as_os_str().to_os_string();
        t.push(".tmp");
        std::path::PathBuf::from(t)
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("mv-state-file-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_atomic_leaves_no_tmp_and_the_new_content() {
        let dir = scratch("basic");
        let path = dir.join("thing-state.tsv");
        write_atomic(&path, "V\t4\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "V\t4\n");
        assert!(
            !tmp_of(&path).exists(),
            "the temp file is renamed away, never left behind"
        );
        // A rewrite REPLACES the content rather than appending to it.
        write_atomic(&path, "V\t4\nK\t1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "V\t4\nK\t1\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_leftover_tmp_from_an_earlier_crash_is_overwritten() {
        let dir = scratch("leftover");
        let path = dir.join("thing-state.tsv");
        let tmp = tmp_of(&path);
        // Exactly what a crash between the write and the rename leaves.
        std::fs::write(&tmp, "half a fi").unwrap();
        write_atomic(&path, "V\t4\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "V\t4\n");
        assert!(!tmp.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unwritable_directory_is_an_error_not_a_panic() {
        let path = Path::new("/no/such/directory/anywhere/thing-state.tsv");
        let e = write_atomic(path, "V\t4\n").expect_err("must fail");
        assert!(e.contains("thing-state"), "the message names the path: {e}");
    }
}
