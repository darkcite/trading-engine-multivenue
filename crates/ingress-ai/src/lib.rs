//! # ingress-ai
//!
//! AI-command ingress (Phase 8f, design §4): a mio-driven Unix-domain
//! socket listener that accepts 82-byte HMAC-tagged frames from
//! `claude-worker`, enforces the §4.4 accept policy, captures every
//! accepted command to PMLR (`SlotKind::AiCmd = 4`) and produces onto
//! the SPSC `Ring<AiCmd, AI_RING_SIZE>` drained by `Engine::tick()`
//! (item 6).
//!
//! ## Thread model (§4.3)
//!
//! One thread owns everything here: listener, at most one client
//! connection, a preallocated 4 KiB rx buffer, the capture sink and
//! the ring producer half. The engine thread is the sole consumer.
//! No shared state beyond the ring and the [`AiIngressStatus`]
//! monitoring slot (single writer per field, `Relaxed`).
//!
//! ## §4.4 accept order (load-bearing)
//!
//! len == 80 → full 82-B frame → HMAC-SHA256 tag16 + constant-time
//! compare (fail ⇒ drop conn) → per-kind shape check (fail ⇒ frame
//! discarded, conn kept) → seq policy (regress discard / gap count) →
//! `ts_ns := now_ns()` rewrite (§13 decision 1) → PMLR capture
//! (BEFORE push, so ring-dropped commands stay auditable) →
//! `try_push` (full ⇒ counted drop) → Stage/Commit side-path seam.
//!
//! **Capture timestamp semantics (operator decision 2026-08-15, S2):**
//! the captured slot is the *rewritten* slot — byte-identical to what
//! the ring consumer sees, engine-clock coherent. The worker's
//! original send time is NOT in the PMLR capture; it survives only in
//! the optional `--raw-tap` payload capture (not hosted by this
//! ingress in 8f). This is the literal §4.4 ordering and supersedes
//! the §13.1 "preserved in PMLR capture" wording (amended in the
//! design doc).
//!
//! ## Zero-copy / zero-alloc accounting (doctrine)
//!
//! Steady state allocates nothing (asserted in
//! `bench/tests/alloc_assertions.rs`). Frames are parsed in place
//! from the rx buffer. The documented copies: the 64-B stack
//! materialization in `AiCmd::read_le` (unaligned rx offsets), the
//! 64-B ring-slot copy on `try_push` (ownership transfer), capture
//! staging (`PmlrWriter` preallocated buffer), and a ≤ 81-B partial
//! -frame compaction in the rx buffer.
//!
//! ## Security (§4.2)
//!
//! Socket dir 0700, socket 0600, stale unlink at bind; single client
//! (second connect accepted-then-closed + counted); peer euid must
//! equal process euid (`LOCAL_PEERCRED` / `SO_PEERCRED`); HMAC per
//! frame on top of peer-cred (defense in depth + audit integrity).
//! The key arrives as `&[u8; 32]` from the cli's `.env` loader
//! (item 6) — this crate never touches the environment.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod capture;
pub mod frame;
pub mod listener;
pub mod ruleset;
pub mod status;

pub use capture::{AiCmdCapture, AI_CMDS_FILE};
pub use frame::{
    len_field, pack_frame, parse_frame, FrameError, SeqPolicy, SeqVerdict, CMD_LEN, CMD_OFFSET,
    FRAME_LEN, LEN_FIELD_VALUE, TAG_LEN, TAG_OFFSET,
};
pub use listener::{
    admit_frame, bind_uds, run, AiIngressCfg, FrameVerdict, POLL_TIMEOUT, RX_BUF_SIZE,
};
pub use ruleset::{
    validate_ruleset, RulesetReject, RulesetSidePath, RULE_EDGE_BPS_MAX, RULE_HORIZON_MS_MAX,
    RULE_HORIZON_MS_MIN, RULE_LEVEL_1E6_MAX, RULE_ROW_MAX_RISK_1E6, RULE_SYM_MAX_RISK_1E6,
    RULE_TABLE_MAX_RISK_1E6,
};
pub use status::AiIngressStatus;
