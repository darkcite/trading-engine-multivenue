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
//! BOOT/OFFLINE DOCTRINE: this runs on the 5 s observability cadence,
//! at shutdown — and, since E6 commit 3, on the execution router's
//! HALT EDGE from the engine thread: once per halt incident, never per
//! poll. The `sync_all` can stall that thread for the length of one
//! disk sync; a halt is a one-shot event and that is accepted.
//! Allocation is fine here, and `exec-router`'s alloc gate 62 measures
//! the edge in its own window so that "once per incident" cannot
//! silently become "once per poll".
//!
//! **XMM XH5c-2 — a writer that runs after init** (slot 6's top-up
//! keeper, on its gateway's thread, at most twice a purchase) takes the
//! temp name once at boot ([`tmp_path`]) and writes through
//! [`write_atomic_at`]: no heap on its success path (std opens a path
//! under its 384 B stack bound without a heap buffer). [`write_atomic`] is
//! the same write with the name taken on the call.

use std::io::Write as _;
use std::path::Path;

/// The temp name [`write_atomic`] stages `path` through: `.tmp`
/// APPENDED to the whole name, never replacing its extension. The
/// original hard-coded `with_extension("tsv.tmp")`, which is right for
/// `*.tsv` and silently wrong for anything else — E4's
/// `exec-budget.state` would have staged through `exec-budget.tsv.tmp`, a
/// name belonging to no file here.
#[must_use]
pub fn tmp_path(path: &Path) -> std::path::PathBuf {
    let name = path.as_os_str();
    // One allocation: the name's room and the suffix's, taken at once.
    let mut tmp = std::ffi::OsString::with_capacity(name.len() + 4);
    tmp.push(name);
    tmp.push(".tmp");
    std::path::PathBuf::from(tmp)
}

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
    write_atomic_at(path, &tmp_path(path), text.as_bytes())
}

/// **XMM XH5c-2** — [`write_atomic`] with its temp name in hand (`tmp`,
/// from [`tmp_path`] at boot): `bytes` to `tmp`, `sync_all`, then the
/// rename over `path`. Its success path takes no heap — the error path
/// formats its message (once, cold).
///
/// # Errors
/// The temp write, the fsync or the rename failed; the message names the
/// path.
pub fn write_atomic_at(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<(), String> {
    {
        let mut f = std::fs::File::create(tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        // The whole point: the bytes reach the disk before the name does.
        f.sync_all()
            .map_err(|e| format!("{}: fsync: {e}", tmp.display()))?;
    }
    std::fs::rename(tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            !tmp_path(&path).exists(),
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
        let tmp = tmp_path(&path);
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

    /// XMM XH5c-2: the temp name is APPENDED, whatever the extension.
    #[test]
    fn the_temp_name_appends_to_the_whole_name() {
        assert_eq!(
            tmp_path(Path::new("/a/xmm-topup-mainnet.state")),
            Path::new("/a/xmm-topup-mainnet.state.tmp")
        );
        assert_eq!(
            tmp_path(Path::new("/a/exec-budget.tsv")),
            Path::new("/a/exec-budget.tsv.tmp")
        );
    }

    /// XMM XH5c-2: the write with its temp name in hand is the same durable
    /// write — the new content, no temp left, a rewrite replacing — and
    /// fails, never panics, where it cannot write.
    #[test]
    fn the_write_with_its_temp_name_in_hand_is_the_same_write() {
        let dir = scratch("at");
        let path = dir.join("xmm-topup-testnet.state");
        let tmp = tmp_path(&path);
        write_atomic_at(&path, &tmp, b"one\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");
        assert!(!tmp.exists());
        write_atomic_at(&path, &tmp, b"two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");
        let gone = dir.join("no-such-dir").join("x.state");
        let e = write_atomic_at(&gone, &tmp_path(&gone), b"x").expect_err("must fail");
        assert!(e.contains("x.state"), "the message names the path: {e}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
