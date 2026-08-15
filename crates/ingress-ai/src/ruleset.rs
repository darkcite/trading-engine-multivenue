//! Phase-8f ruleset stage/commit side-path **stub** (design §7 "8f
//! ruleset side-path (stub scope)", S6 item 14).
//!
//! Sits behind the §4.4 step-8 seam: the listener routes accepted
//! `RulesetStage` / `RulesetCommit` commands here after `try_push`.
//! Scope in 8f (S6 kickoff supersedes the fuller §7 paragraph — the
//! JSON shape bounds-check and the double-buffered table flip belong
//! to the 8g `strategy-vm` evaluator):
//!
//! * **Stage**: resolve `AI_RULESET_DIR/<hash128-hex>.json` — the frame
//!   carries only hash128 (§13 decision 5), so the filename MUST be
//!   derivable from it; the convention (first 32 hex chars of the full
//!   SHA-256) is taught in `docs/prompts/ai-session.md` §4 step 5 —
//!   read the file, recompute the FULL SHA-256 (`core-crypto`), and
//!   require its first 16 bytes to equal the frame's hash128. Match ⇒
//!   staged state + `engine_ai_ruleset_staged_total`; missing file /
//!   I/O error / hash mismatch ⇒ `engine_ai_ruleset_rejected_total`.
//! * **Commit**: valid only for the currently staged hash ⇒ committed
//!   state flag (observable via `engine_ai_ruleset_committed_total`);
//!   anything else ⇒ rejected. A later successful Stage supersedes a
//!   Commit (committed state clears) — the worker-side registry
//!   (`state.py`) mirrors exactly this machine.
//! * **Table-fill stub**: on a successful Stage the validated bytes are
//!   dropped after hashing; 8g replaces the drop with ruleset parsing
//!   into the strategy-vm double buffer and flips it on Commit.
//!
//! Allocation note (doctrine): `PathBuf::join` and `fs::read` allocate.
//! This path runs at **operator cadence** — only Stage/Commit kinds are
//! routed here, never market data — and executes after the frame has
//! already been captured and pushed; the 0 B/op alloc gate covers the
//! admit→verify→capture→push pump, which remains allocation-free.

use std::path::PathBuf;
use std::sync::Arc;

use core_crypto::sha256;
use core_types::{AiCmd, AiCmdKind};

use crate::status::AiIngressStatus;

/// Bytes of the truncated ruleset identity carried in `px`+`qty`
/// (§13 decision 5).
pub const HASH128_LEN: usize = 16;

/// File-name suffix of ruleset artifacts in `AI_RULESET_DIR`.
const SUFFIX: &[u8; 5] = b".json";

/// Stage/commit side-path state. Owned by the ingress-ai thread (the
/// seam closure captures it); counters live in the shared
/// [`AiIngressStatus`] slot so the cli mirrors the whole family from
/// one place. Single-writer: only the ingress thread touches this.
pub struct RulesetSidePath {
    dir: PathBuf,
    status: Arc<AiIngressStatus>,
    staged: Option<[u8; HASH128_LEN]>,
    committed: Option<[u8; HASH128_LEN]>,
}

impl RulesetSidePath {
    /// New side-path rooted at `dir` (`AI_RULESET_DIR`, tilde already
    /// expanded by config). The directory is not required to exist at
    /// boot — a Stage against a missing dir is just a rejected stage.
    pub fn new(dir: PathBuf, status: Arc<AiIngressStatus>) -> Self {
        Self {
            dir,
            status,
            staged: None,
            committed: None,
        }
    }

    /// Currently staged hash128, if any (test/diagnostic surface).
    #[inline]
    pub fn staged(&self) -> Option<[u8; HASH128_LEN]> {
        self.staged
    }

    /// Currently committed hash128, if any (test/diagnostic surface).
    #[inline]
    pub fn committed(&self) -> Option<[u8; HASH128_LEN]> {
        self.committed
    }

    /// Reassemble the wire hash128 from the `px`+`qty` halves
    /// (little-endian signed halves; byte convention pinned by the
    /// shared golden vectors).
    #[inline]
    fn cmd_hash128(cmd: &AiCmd) -> [u8; HASH128_LEN] {
        let mut h = [0u8; HASH128_LEN];
        h[..8].copy_from_slice(&cmd.px.to_le_bytes());
        h[8..].copy_from_slice(&cmd.qty.to_le_bytes());
        h
    }

    /// `<hash128-hex>.json` as a fixed stack buffer (37 ASCII bytes).
    fn file_name(hash128: &[u8; HASH128_LEN]) -> [u8; 2 * HASH128_LEN + SUFFIX.len()] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = [0u8; 2 * HASH128_LEN + SUFFIX.len()];
        let mut i = 0;
        while i < HASH128_LEN {
            name[2 * i] = HEX[(hash128[i] >> 4) as usize];
            name[2 * i + 1] = HEX[(hash128[i] & 0x0f) as usize];
            i += 1;
        }
        name[2 * HASH128_LEN..].copy_from_slice(SUFFIX);
        name
    }

    /// Seam entry point (§4.4 step 8). Non-ruleset kinds are never
    /// routed here by the listener; they no-op defensively.
    pub fn on_cmd(&mut self, cmd: &AiCmd) {
        match cmd.kind() {
            Some(AiCmdKind::RulesetStage) => self.stage(Self::cmd_hash128(cmd)),
            Some(AiCmdKind::RulesetCommit) => self.commit(Self::cmd_hash128(cmd)),
            _ => {}
        }
    }

    fn stage(&mut self, hash128: [u8; HASH128_LEN]) {
        let name = Self::file_name(&hash128);
        // SAFETY: `name` is built exclusively from ASCII hex digits and
        // the ASCII ".json" suffix — always valid UTF-8.
        let name_str = unsafe { core::str::from_utf8_unchecked(&name) };
        let file = self.dir.join(name_str);
        match std::fs::read(&file) {
            Ok(bytes) => {
                let digest = sha256(&bytes);
                if digest[..HASH128_LEN] == hash128 {
                    // 8g table-fill stub: parse `bytes` into the
                    // strategy-vm double buffer HERE; 8f drops them
                    // after the hash check (module docs).
                    self.staged = Some(hash128);
                    // A new Stage supersedes any previous Commit —
                    // the worker registry mirrors this (state.py).
                    self.committed = None;
                    self.status.inc_ruleset_staged();
                } else {
                    self.status.inc_ruleset_rejected();
                }
            }
            Err(_) => self.status.inc_ruleset_rejected(),
        }
    }

    fn commit(&mut self, hash128: [u8; HASH128_LEN]) {
        if self.staged == Some(hash128) {
            // The 8f "state flag" (§7): observable through
            // `engine_ai_ruleset_committed_total`; 8g flips the
            // evaluator's double buffer at this point.
            self.committed = Some(hash128);
            self.status.inc_ruleset_committed();
        } else {
            self.status.inc_ruleset_rejected();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{VenueId, AI_SIDE_NONE, STRATEGY_SLOT_VM, SYMBOL_ID_NONE};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cw-ai-ruleset-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp ruleset dir");
        dir
    }

    fn ruleset_cmd(kind: AiCmdKind, hash128: [u8; HASH128_LEN]) -> AiCmd {
        let px = i64::from_le_bytes(hash128[..8].try_into().expect("8 bytes"));
        let qty = i64::from_le_bytes(hash128[8..].try_into().expect("8 bytes"));
        AiCmd::new(
            11,
            1,
            SYMBOL_ID_NONE,
            px,
            qty,
            0,
            kind,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    fn install(dir: &PathBuf, bytes: &[u8]) -> [u8; HASH128_LEN] {
        let digest = sha256(bytes);
        let mut h = [0u8; HASH128_LEN];
        h.copy_from_slice(&digest[..HASH128_LEN]);
        let name = RulesetSidePath::file_name(&h);
        let path = dir.join(core::str::from_utf8(&name).expect("ascii"));
        std::fs::write(path, bytes).expect("write ruleset artifact");
        h
    }

    #[test]
    fn stage_then_commit_happy_path() {
        let dir = temp_dir("happy");
        let status = Arc::new(AiIngressStatus::new());
        let mut side = RulesetSidePath::new(dir.clone(), Arc::clone(&status));
        let h = install(&dir, br#"{"rows":[]}"#);

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h));
        assert_eq!(status.ruleset_staged(), 1);
        assert_eq!(status.ruleset_rejected(), 0);
        assert_eq!(side.staged(), Some(h));
        assert_eq!(side.committed(), None);

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, h));
        assert_eq!(status.ruleset_committed(), 1);
        assert_eq!(side.committed(), Some(h));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stage_missing_file_is_rejected() {
        let dir = temp_dir("missing");
        let status = Arc::new(AiIngressStatus::new());
        let mut side = RulesetSidePath::new(dir.clone(), Arc::clone(&status));

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, [0x11; HASH128_LEN]));
        assert_eq!(status.ruleset_staged(), 0);
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(side.staged(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stage_hash_mismatch_is_rejected() {
        let dir = temp_dir("mismatch");
        let status = Arc::new(AiIngressStatus::new());
        let mut side = RulesetSidePath::new(dir.clone(), Arc::clone(&status));
        // File exists under the claimed name but its bytes hash
        // differently — a tampered/mis-installed artifact.
        let h = install(&dir, br#"{"rows":[1]}"#);
        let name = RulesetSidePath::file_name(&h);
        let path = dir.join(core::str::from_utf8(&name).expect("ascii"));
        std::fs::write(path, b"tampered").expect("overwrite artifact");

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h));
        assert_eq!(status.ruleset_staged(), 0);
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(side.staged(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_unstaged_or_wrong_hash_is_rejected() {
        let dir = temp_dir("commit-reject");
        let status = Arc::new(AiIngressStatus::new());
        let mut side = RulesetSidePath::new(dir.clone(), Arc::clone(&status));

        // Nothing staged at all.
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, [0x22; HASH128_LEN]));
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(side.committed(), None);

        // Staged, but the commit names a different hash.
        let h = install(&dir, br#"{"rows":[2]}"#);
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h));
        assert_eq!(status.ruleset_staged(), 1);
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, [0x33; HASH128_LEN]));
        assert_eq!(status.ruleset_rejected(), 2);
        assert_eq!(side.committed(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restage_supersedes_commit() {
        let dir = temp_dir("restage");
        let status = Arc::new(AiIngressStatus::new());
        let mut side = RulesetSidePath::new(dir.clone(), Arc::clone(&status));
        let h1 = install(&dir, br#"{"rows":[3]}"#);
        let h2 = install(&dir, br#"{"rows":[4]}"#);

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h1));
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, h1));
        assert_eq!(side.committed(), Some(h1));

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h2));
        assert_eq!(side.staged(), Some(h2));
        assert_eq!(side.committed(), None, "a new Stage supersedes the Commit");
        assert_eq!(status.ruleset_staged(), 2);
        assert_eq!(status.ruleset_committed(), 1);
        assert_eq!(status.ruleset_rejected(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_ruleset_kind_is_a_no_op() {
        let dir = temp_dir("noop");
        let status = Arc::new(AiIngressStatus::new());
        let mut side = RulesetSidePath::new(dir.clone(), Arc::clone(&status));
        let cmd = AiCmd::new(
            11,
            1,
            SYMBOL_ID_NONE,
            0,
            0,
            0,
            AiCmdKind::Heartbeat,
            VenueId::Ai,
            0xFF,
            AI_SIDE_NONE,
            0,
            0,
        );
        side.on_cmd(&cmd);
        assert_eq!(status.ruleset_staged(), 0);
        assert_eq!(status.ruleset_committed(), 0);
        assert_eq!(status.ruleset_rejected(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_name_is_hash128_hex_json() {
        let mut h = [0u8; HASH128_LEN];
        h[0] = 0x10;
        h[1] = 0x32;
        h[15] = 0x01;
        let name = RulesetSidePath::file_name(&h);
        assert_eq!(
            core::str::from_utf8(&name).expect("ascii"),
            "10320000000000000000000000000001.json"
        );
    }
}
