//! Single-file PMLR capture sink for accepted AI commands.
//!
//! `PmlrCapture` (core-io) is the three-file *venue* sink
//! (ticks/events/signals); the AI ingress captures exactly one slot
//! type, so it gets a dedicated single-file sink writing
//! `ai-cmds.pmlr` (`SlotKind::AiCmd = 4`) with the same error policy:
//!
//! * Capture is observability — an I/O error **sticky-disables** the
//!   sink (`debug_assert!` in debug builds), counts it, and every
//!   later hook is a no-op branch. It must never take down the
//!   ingress thread.
//! * All allocation happens in [`AiCmdCapture::open`] (boot). Steady
//!   state appends copy one 64-B slot into the `PmlrWriter` staging
//!   buffer; flushes are plain `write_all` syscalls.
//! * Flush cadence mirrors core-io: staged bytes reach disk at least
//!   every [`core_io::capture::CAPTURE_FLUSH_INTERVAL_NS`].
//!
//! Captured slots carry the **rewritten** `ts_ns` (engine-monotonic
//! accept time) — byte-identical to what the ring consumer sees
//! (operator decision 2026-08-15, S2: literal §4.4 ordering). The
//! worker's original send time survives only in the optional
//! `--raw-tap` payload capture, which the AI ingress does not host in
//! 8f.
//!
//! Since item 6 this is a thin wrapper over the hoisted
//! `core_io::SlotCapture` (the S2 handoff flagged the hoist for the
//! moment a second single-file user appeared — the engine fills
//! capture is that user). Public API and policy are unchanged.

use std::io;
use std::path::Path;

use core_io::{SlotCapture, SlotKind};
use core_types::AiCmd;

/// File name of the AI-command capture inside the per-run capture
/// directory (`<MULTIVENUE_LOG_DIR>/run-<epoch_ns>/`).
pub const AI_CMDS_FILE: &str = "ai-cmds.pmlr";

/// Single-file capture sink for [`AiCmd`] slots. See module docs.
pub struct AiCmdCapture {
    inner: SlotCapture<AiCmd>,
}

impl AiCmdCapture {
    /// Create `dir/ai-cmds.pmlr` (directory created if absent).
    /// `epoch_ns` is wall-clock ns at open (PMLR header contract).
    /// Boot-time only — allocates.
    pub fn open<P: AsRef<Path>>(dir: P, epoch_ns: u64) -> io::Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let inner = SlotCapture::open(dir.join(AI_CMDS_FILE), SlotKind::AiCmd, epoch_ns)?;
        Ok(Self { inner })
    }

    /// Stage one accepted command (§4.4 step 6 — called BEFORE the
    /// ring `try_push`, so ring-dropped commands remain auditable).
    #[inline]
    pub fn append(&mut self, cmd: &AiCmd) {
        self.inner.append(cmd);
    }

    /// Drain staging to disk if the flush interval has elapsed.
    #[inline]
    pub fn maybe_flush(&mut self, now_ns: u64) {
        self.inner.maybe_flush(now_ns);
    }

    /// Unconditional drain — orderly shutdown path.
    pub fn flush_all(&mut self) -> io::Result<()> {
        self.inner.flush_all()
    }

    /// Slots staged since open (mirrored into
    /// `engine_ingress_ai_capture_records` by the cli, item 6).
    #[inline]
    pub fn records(&self) -> u64 {
        self.inner.records()
    }

    /// I/O errors observed (first one sticky-disables; mirrored into
    /// `engine_ingress_ai_capture_io_errors`).
    #[inline]
    pub fn io_errors(&self) -> u64 {
        self.inner.io_errors()
    }

    /// True once an I/O error has sticky-disabled this sink.
    #[inline]
    pub fn is_disabled(&self) -> bool {
        self.inner.is_disabled()
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_io::capture::CAPTURE_FLUSH_INTERVAL_NS;
    use core_io::PmlrReader;
    use core_types::{AiCmdKind, VenueId, AI_SIDE_NONE, STRATEGY_SLOT_NONE, SYMBOL_ID_NONE};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("stage2_ai_capture_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn heartbeat(seq: u32) -> AiCmd {
        AiCmd::new(
            5,
            seq,
            SYMBOL_ID_NONE,
            0,
            0,
            0,
            AiCmdKind::Heartbeat,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    #[test]
    fn append_flush_read_roundtrips() {
        let dir = temp_dir("roundtrip");
        let mut c = AiCmdCapture::open(&dir, 123).unwrap();
        c.append(&heartbeat(1));
        c.append(&heartbeat(2));
        c.flush_all().unwrap();
        assert_eq!(c.records(), 2);
        assert_eq!(c.io_errors(), 0);
        assert!(!c.is_disabled());

        let r = PmlrReader::<AiCmd>::open(dir.join(AI_CMDS_FILE)).unwrap();
        assert_eq!(r.slot_kind(), SlotKind::AiCmd);
        assert_eq!(r.epoch_ns(), 123);
        assert_eq!(r.len(), 2);
        assert_eq!(r.records()[0].seq, 1);
        assert_eq!(r.records()[1].seq, 2);
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maybe_flush_respects_interval() {
        let dir = temp_dir("interval");
        let mut c = AiCmdCapture::open(&dir, 0).unwrap();
        c.append(&heartbeat(1));
        c.maybe_flush(CAPTURE_FLUSH_INTERVAL_NS - 1);
        assert_eq!(
            std::fs::metadata(dir.join(AI_CMDS_FILE)).unwrap().len(),
            64,
            "below interval: header only"
        );
        c.maybe_flush(CAPTURE_FLUSH_INTERVAL_NS);
        assert_eq!(
            std::fs::metadata(dir.join(AI_CMDS_FILE)).unwrap().len(),
            128,
            "interval reached: slot drained"
        );
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_drains_staging() {
        let dir = temp_dir("drop");
        {
            let mut c = AiCmdCapture::open(&dir, 0).unwrap();
            c.append(&heartbeat(7));
            // No explicit flush.
        }
        let r = PmlrReader::<AiCmd>::open(dir.join(AI_CMDS_FILE)).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r.records()[0].seq, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_fails_on_unwritable_dir() {
        let base = temp_dir("unwritable");
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(AiCmdCapture::open(&blocker, 0).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
