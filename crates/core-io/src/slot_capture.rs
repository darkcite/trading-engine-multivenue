//! # SlotCapture — generic single-file PMLR capture sink
//!
//! [`crate::PmlrCapture`] is the three-file *venue* sink
//! (ticks/events/signals + optional raw tap). Some capture users
//! stage exactly one slot type into exactly one file:
//!
//! * `ingress-ai` → `ai-cmds.pmlr` ([`SlotKind::AiCmd`], Phase 8f
//!   item 5 — `ingress_ai::AiCmdCapture` is a thin wrapper over this
//!   type since item 6),
//! * the engine thread → `engine-fills.pmlr` ([`SlotKind::Fill`],
//!   Phase 8f item 6 — the positions/P&L feed for the research loop).
//!
//! This type is that shape, hoisted here per the S2 handoff note ("do
//! not duplicate a third time"). Policy is identical to
//! [`crate::PmlrCapture`]:
//!
//! * **Sticky-disable on I/O error.** Capture is observability — an
//!   error (`debug_assert!` in debug builds) counts, disables the
//!   sink, and every later hook is a no-op branch. It must never take
//!   down the owning thread.
//! * **Boot-only allocation.** [`SlotCapture::open`] allocates; the
//!   steady-state [`SlotCapture::append`] copies one 64 B slot into
//!   the [`PmlrWriter`] staging buffer; flushes are plain `write_all`
//!   syscalls.
//! * **Flush cadence.** Staged bytes reach disk at least every
//!   [`crate::capture::CAPTURE_FLUSH_INTERVAL_NS`] *provided the
//!   owner keeps calling* [`SlotCapture::maybe_flush`]; a final drain
//!   runs on drop.
//!
//! The `R: AsBytes` parameter pins one slot type per file at the type
//! level — a `SlotCapture<Fill>` cannot be fed an `AiCmd`, which a
//! bare method-generic `PmlrWriter::append` would happily corrupt the
//! file with.

use std::io;
use std::marker::PhantomData;
use std::path::Path;

use core_types::AsBytes;

use crate::capture::CAPTURE_FLUSH_INTERVAL_NS;
use crate::pmlr::{PmlrWriter, SlotKind};

/// Generic single-file capture sink for one 64 B slot type. See the
/// module docs for policy; see `ingress_ai::AiCmdCapture` and the
/// engine's fills capture for the two Phase-8f users.
pub struct SlotCapture<R: AsBytes> {
    w: PmlrWriter,
    enabled: bool,
    io_errors: u64,
    last_flush_ns: u64,
    _slot: PhantomData<R>,
}

impl<R: AsBytes> SlotCapture<R> {
    /// Create/truncate the PMLR file at `path` (parent directory must
    /// already exist — callers that own a directory layout create it
    /// first). `epoch_ns` is wall-clock ns at open (PMLR header
    /// contract). Boot-time only — allocates.
    pub fn open<P: AsRef<Path>>(path: P, kind: SlotKind, epoch_ns: u64) -> io::Result<Self> {
        let w = PmlrWriter::open(path, kind, epoch_ns)?;
        Ok(Self {
            w,
            enabled: true,
            io_errors: 0,
            last_flush_ns: 0,
            _slot: PhantomData,
        })
    }

    /// Stage one slot. Zero-alloc; no-op branch once sticky-disabled.
    #[inline]
    pub fn append(&mut self, slot: &R) {
        if !self.enabled {
            return;
        }
        if self.w.append(slot).is_err() {
            self.note_io_error();
        }
    }

    /// Drain staging to disk if the flush interval has elapsed since
    /// the last drain. `now_ns` is the caller's monotonic clock — the
    /// sink never reads the clock itself.
    #[inline]
    pub fn maybe_flush(&mut self, now_ns: u64) {
        if !self.enabled {
            return;
        }
        if now_ns.wrapping_sub(self.last_flush_ns) < CAPTURE_FLUSH_INTERVAL_NS {
            return;
        }
        self.last_flush_ns = now_ns;
        if self.w.flush().is_err() {
            self.note_io_error();
        }
    }

    /// Unconditional drain — orderly shutdown path.
    pub fn flush_all(&mut self) -> io::Result<()> {
        self.w.flush()
    }

    /// Slots staged since open (monotonic; includes slots still in
    /// staging). Mirrored into the owner's `*_capture_records` gauge.
    #[inline]
    pub fn records(&self) -> u64 {
        self.w.records_written()
    }

    /// I/O errors observed (the first one sticky-disables; mirrored
    /// into the owner's `*_capture_io_errors` gauge).
    #[inline]
    pub fn io_errors(&self) -> u64 {
        self.io_errors
    }

    /// True once an I/O error has sticky-disabled this sink.
    #[inline]
    pub fn is_disabled(&self) -> bool {
        !self.enabled
    }

    /// Sticky-disable on error (PmlrCapture policy).
    #[cold]
    fn note_io_error(&mut self) {
        self.io_errors += 1;
        self.enabled = false;
        debug_assert!(false, "slot capture I/O error — sticky-disabled");
    }
}

impl<R: AsBytes> Drop for SlotCapture<R> {
    fn drop(&mut self) {
        // Best-effort final drain; the process is tearing down.
        let _ = self.flush_all();
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PmlrReader;
    use core_types::{Fill, Price, Qty, Side};

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "stage2_slot_capture_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("engine-fills.pmlr")
    }

    fn fill(seq: u64) -> Fill {
        Fill::new(
            seq,
            7,
            Side::Bid,
            Price::from_raw(500_000),
            Qty::from_raw(1_000_000),
            seq,
        )
    }

    #[test]
    fn append_flush_read_roundtrips() {
        let p = temp_path("roundtrip");
        let mut c: SlotCapture<Fill> = SlotCapture::open(&p, SlotKind::Fill, 42).unwrap();
        c.append(&fill(1));
        c.append(&fill(2));
        c.flush_all().unwrap();
        assert_eq!(c.records(), 2);
        assert_eq!(c.io_errors(), 0);
        assert!(!c.is_disabled());

        let r = PmlrReader::<Fill>::open(&p).unwrap();
        assert_eq!(r.slot_kind(), SlotKind::Fill);
        assert_eq!(r.epoch_ns(), 42);
        assert_eq!(r.len(), 2);
        assert_eq!(r.records()[0].ts_ns, 1);
        assert_eq!(r.records()[1].ts_ns, 2);
        drop(c);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn maybe_flush_respects_interval() {
        let p = temp_path("interval");
        let mut c: SlotCapture<Fill> = SlotCapture::open(&p, SlotKind::Fill, 0).unwrap();
        c.append(&fill(1));
        c.maybe_flush(CAPTURE_FLUSH_INTERVAL_NS - 1);
        assert_eq!(
            std::fs::metadata(&p).unwrap().len(),
            64,
            "below interval: header only"
        );
        c.maybe_flush(CAPTURE_FLUSH_INTERVAL_NS);
        assert_eq!(
            std::fs::metadata(&p).unwrap().len(),
            128,
            "interval reached: slot drained"
        );
        drop(c);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn drop_drains_staging() {
        let p = temp_path("drop");
        {
            let mut c: SlotCapture<Fill> = SlotCapture::open(&p, SlotKind::Fill, 0).unwrap();
            c.append(&fill(9));
            // No explicit flush.
        }
        let r = PmlrReader::<Fill>::open(&p).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r.records()[0].ts_ns, 9);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn open_fails_on_missing_parent_dir() {
        let p = std::env::temp_dir()
            .join(format!("stage2_slot_capture_missing_{}", std::process::id()))
            .join("nope")
            .join("engine-fills.pmlr");
        assert!(SlotCapture::<Fill>::open(&p, SlotKind::Fill, 0).is_err());
    }
}
