// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 (lead rulings 2026-10-17): slot 8's typed engine-loop hook ([`PineHook`], a
//! [`crate::paper::LoopHook`] on the concrete `StrategySet`) — no strategy-core / strategy-set
//! trait change.
//!
//! - **The boot record** (impl-Bp §3.10, R5): right after `eng.start()`,
//!   `set.pine().boot_record(run_epoch_ns, boot_mono_ns)` is written once, atomically, to
//!   `<run dir>/pine-boot.tsv` (`pine_boot::write_boot_record`).
//! - **The delivery journal** (R6): with `--pine-tape` only, the member's ring
//!   (`PineStrategy::delivery_view`) is drained on the 5 s report block, and once more when the
//!   loop ends, into `<run dir>/pine-delivery.bin` — raw 32-byte records, append-only. A gap (an
//!   overrun the member counted) is logged: the window is then ineligible.
//!
//! COPY-DOCTRINE: boot-time and 5 s-cadence operator I/O, never on the decision path; it may
//! allocate at `open` (paths, the drain buffer) and copies records into fixed byte images.
//!
//! Literal readings:
//! - pin:08-delivery-bytes-le: a record is written as its `#[repr(C)]` image, field by field in
//!   little-endian, padding as zeros — 32 B per `PineDeliveryRec` (the tape writer's rule,
//!   pin:08-tape-bytes-le); no `unsafe` byte view.
//! - pin:08-boot-record-write-logged: a boot record that cannot be written is logged at ERROR and
//!   the run continues (the tape writer's precedent: an evidence file never refuses a boot); the
//!   run then has no record and impl-Bp's replay-from-boot refuses it.
//! - pin:08-tape-buffered (risk r21 #3): the journal is a `BufWriter` of
//!   [`crate::pine_tape::TAPE_BUF_BYTES`] allocated at `open`; each drain (5 s and final) writes
//!   into it and flushes once at its end. A write or flush error is logged; the window is void.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use strategy_pine::delivery::PineDeliveryRec;
use strategy_pine::PineStrategy;

/// The journal's file name in the run directory.
pub const DELIVERY_FILE: &str = "pine-delivery.bin";
/// One record's size on disk.
pub const REC_BYTES: usize = 32;
/// Records drained per view call.
pub const DRAIN_BATCH: usize = 256;

/// One record as its 32-byte image (pin:08-delivery-bytes-le).
#[must_use]
pub fn rec_bytes(r: &PineDeliveryRec) -> [u8; REC_BYTES] {
    let mut b = [0u8; REC_BYTES];
    b[0..8].copy_from_slice(&r.ts_ns.to_le_bytes());
    b[8..16].copy_from_slice(&r.seq.to_le_bytes());
    b[16..20].copy_from_slice(&r.sym.to_le_bytes());
    b[20] = r.kind;
    b[21] = r.venue;
    // 22..24: padding (zeros).
    b[24..32].copy_from_slice(&r.aux.to_le_bytes());
    b
}

fn le_u64(b: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    // COPY: 8 B into a word — offline reader — `from_le_bytes` takes an array by value.
    w.copy_from_slice(&b[..8]);
    u64::from_le_bytes(w)
}

/// One 32-byte image back to its record (the inverse of [`rec_bytes`]).
#[must_use]
pub fn rec_from_bytes(b: &[u8; REC_BYTES]) -> PineDeliveryRec {
    let mut s = [0u8; 4];
    // COPY: 4 B into a word — offline reader — as `le_u64`.
    s.copy_from_slice(&b[16..20]);
    PineDeliveryRec {
        ts_ns: le_u64(&b[0..8]),
        seq: le_u64(&b[8..16]),
        sym: u32::from_le_bytes(s),
        kind: b[20],
        venue: b[21],
        _pad: [0; 2],
        aux: le_u64(&b[24..32]),
    }
}

/// Read a whole journal file (offline; allocates). A trailing partial record is refused.
///
/// # Errors
/// The file cannot be read, or its length is not a multiple of 32.
pub fn read_journal(path: &Path) -> io::Result<Vec<PineDeliveryRec>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() % REC_BYTES != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: {} bytes is not a whole number of 32-byte records",
                path.display(),
                bytes.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / REC_BYTES);
    for c in bytes.chunks_exact(REC_BYTES) {
        let mut a = [0u8; REC_BYTES];
        // COPY: one 32 B record — offline reader — `chunks_exact` lends a slice, not an array.
        a.copy_from_slice(c);
        out.push(rec_from_bytes(&a));
    }
    Ok(out)
}

/// What one drain wrote and what it found missing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeliveryDrain {
    /// Records written.
    pub written: u64,
    /// Records lost to an overrun before this drain read them (the window is void).
    pub gap: u64,
}

/// `<run dir>/pine-delivery.bin`, append-only, and the read cursor.
#[derive(Debug)]
pub struct DeliveryWriter {
    path: PathBuf,
    file: BufWriter<File>,
    next: u64,
    buf: Box<[PineDeliveryRec]>,
}

impl DeliveryWriter {
    /// Create (or append to) the run's journal. Boot-only.
    ///
    /// # Errors
    /// The file cannot be opened.
    pub fn open(run_dir: &Path) -> io::Result<Self> {
        let path = run_dir.join(DELIVERY_FILE);
        let file = BufWriter::with_capacity(
            crate::pine_tape::TAPE_BUF_BYTES,
            OpenOptions::new().create(true).append(true).open(&path)?,
        );
        Ok(Self {
            path,
            file,
            next: 0,
            buf: vec![PineDeliveryRec::default(); DRAIN_BATCH].into_boxed_slice(),
        })
    }

    /// The journal's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Drain the member's ring until empty; a gap is counted, never hidden.
    ///
    /// # Errors
    /// A write failed.
    pub fn drain(&mut self, p: &PineStrategy) -> io::Result<DeliveryDrain> {
        let mut o = DeliveryDrain::default();
        loop {
            let (first, n) = p.delivery_view(&mut self.buf, self.next);
            if n == 0 {
                break;
            }
            o.gap += first.saturating_sub(self.next);
            for r in &self.buf[..n as usize] {
                self.file.write_all(&rec_bytes(r))?;
            }
            self.next = first + u64::from(n);
            o.written += u64::from(n);
        }
        // pin:08-tape-buffered: one flush per drain.
        self.file.flush()?;
        Ok(o)
    }
}

/// Slot 8's loop hook: the boot record at start, the journal's drains.
#[derive(Debug)]
pub struct PineHook {
    run_dir: PathBuf,
    run_epoch_ns: u64,
    tape: bool,
    writer: Option<DeliveryWriter>,
    record: Option<PathBuf>,
    /// pin:08-verify-queue-sym-ineligible: the symbols the paper queue law tracks at this boot.
    queue_syms: Vec<core_types::SymbolId>,
}

impl PineHook {
    /// `run_dir` / `run_epoch_ns`: the run's capture directory and its epoch
    /// (`new_capture_run_dir`); `tape`: `--pine-tape` (the journal is written only with it).
    #[must_use]
    pub fn new(run_dir: PathBuf, run_epoch_ns: u64, tape: bool) -> Self {
        Self {
            run_dir,
            run_epoch_ns,
            tape,
            writer: None,
            record: None,
            queue_syms: Vec::new(),
        }
    }

    /// Boot-time, before `eng.start()`: one symbol the paper dispatcher's queue law tracks
    /// (`track_queue_sym`) — the boot record lists them, and `pine-verify` refuses a window whose
    /// pine symbol is among them (pin:08-verify-queue-sym-ineligible).
    pub fn note_queue_sym(&mut self, sym: core_types::SymbolId) {
        if !self.queue_syms.contains(&sym) {
            self.queue_syms.push(sym);
        }
    }

    /// Boot-time, once, right after `eng.start()`: write the boot record and, with `--pine-tape`,
    /// open the journal. Returns the record's path (`None` when it could not be written,
    /// pin:08-boot-record-write-logged).
    pub fn on_started(&mut self, p: &PineStrategy, boot_mono_ns: u64) -> Option<&Path> {
        let rec = p.boot_record(self.run_epoch_ns, boot_mono_ns).map(|mut r| {
            // COPY: the queue symbols (≤ 8) into the owned record — boot only — the record
            // renders from its own fields.
            r.queue_syms.clone_from(&self.queue_syms);
            r
        });
        // The set ran slot 8's `on_start` (`eng.start()` returned Ok), so the core exists.
        debug_assert!(rec.is_some(), "pine: boot record before on_start");
        match rec.map(|r| crate::pine_boot::write_boot_record(&self.run_dir, &r)) {
            Some(Ok(path)) => {
                tracing::info!(path = %path.display(), "pine: boot record written");
                self.record = Some(path);
            }
            Some(Err(reason)) => tracing::error!(
                %reason,
                "pine: boot record NOT written — this run cannot be replayed from boot"
            ),
            None => tracing::error!("pine: no boot record (slot 8 not started)"),
        }
        if self.tape {
            match DeliveryWriter::open(&self.run_dir) {
                Ok(w) => self.writer = Some(w),
                Err(e) => tracing::error!(
                    error = %e,
                    "pine: delivery journal not writable — window void"
                ),
            }
        }
        self.record.as_deref()
    }

    /// The 5 s block (and the loop's end): drain the journal, logging a gap or a write failure.
    pub fn on_drain(&mut self, p: &PineStrategy) -> Option<DeliveryDrain> {
        let w = self.writer.as_mut()?;
        match w.drain(p) {
            Ok(o) => {
                if o.gap > 0 {
                    tracing::error!(gap = o.gap, "pine: delivery journal gap — window void");
                }
                Some(o)
            }
            Err(e) => {
                tracing::error!(error = %e, "pine: delivery journal write failed — window void");
                None
            }
        }
    }

    /// The journal's path, once opened.
    #[must_use]
    pub fn journal_path(&self) -> Option<&Path> {
        self.writer.as_ref().map(DeliveryWriter::path)
    }
}

impl crate::paper::LoopHook<strategy_set::StrategySet> for PineHook {
    fn started(&mut self, strat: &strategy_set::StrategySet, mono_ns: u64) {
        let _ = self.on_started(strat.pine(), mono_ns);
    }

    fn drain(&mut self, strat: &strategy_set::StrategySet) {
        let _ = self.on_drain(strat.pine());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_image_round_trips() {
        let r = PineDeliveryRec {
            ts_ns: 0x0102_0304_0506_0708,
            seq: u64::MAX - 3,
            sym: 0xA0B0_C0D0,
            kind: 2,
            venue: 9,
            _pad: [0; 2],
            aux: 0x8000_0000_0000_0001,
        };
        let b = rec_bytes(&r);
        assert_eq!(&b[0..8], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&b[22..24], &[0, 0]);
        assert_eq!(rec_from_bytes(&b), r);
    }

    /// pin:08-verify-queue-sym-ineligible (risk r21 #4), structurally: the paper boot reaches the
    /// queue law only through `track_queue_sym_kept`, and hands the kept symbols to slot 8's hook
    /// after the last member block's use and right before the loop — whatever the block order.
    #[test]
    fn queue_syms_reach_the_boot_record_whatever_the_block_order() {
        let src = include_str!("paper.rs");
        let at = |pat: &str| src.find(pat).unwrap_or_else(|| panic!("{pat}"));
        assert_eq!(src.matches(".track_queue_sym(").count(), 1, "one door");
        assert!(at(".track_queue_sym(") > at("fn track_queue_sym_kept<"));
        let last_use = src.rfind("track_queue_sym_kept(&mut disp").expect("a use");
        let hand = at("h.note_queue_sym(*s);");
        let run = at("run_engine_loop(cons, disp, set, obs, pine_hook)");
        assert!(last_use < hand && hand < run, "{last_use} {hand} {run}");
        assert_eq!(
            src.matches("note_queue_sym(").count(),
            1,
            "only the hand-off"
        );
    }

    #[test]
    fn a_partial_record_is_refused() {
        let dir = std::env::temp_dir().join(format!("pine-delivery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let p = dir.join(DELIVERY_FILE);
        std::fs::write(&p, [0u8; 33]).expect("write");
        assert!(read_journal(&p).is_err());
        std::fs::write(&p, [0u8; 64]).expect("write");
        assert_eq!(read_journal(&p).expect("read").len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
