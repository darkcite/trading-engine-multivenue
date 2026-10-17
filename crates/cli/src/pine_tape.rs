// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 T18 (cli half): the `--pine-tape` writer. The 5 s block drains slot 8's two rings
//! (`pine_seq_view`, `pine_tape_view`) into `<root>/<boot_utc_ms>/seq.bin` and `decisions.bin`
//! (raw records, append-only), and appends one `set.tsv` line (§4.9.2). A gap in either ring's
//! `seq` (an overrun the member counted) is returned so the caller can void the window (I44).
//!
//! COPY-DOCTRINE: cold operator I/O on the 5 s cadence, never on the decision path; it may
//! allocate at `open` (paths) and copies records into its fixed byte buffers.
//!
//! Literal readings:
//! - pin:08-tape-bytes-le: a record is written as its `#[repr(C)]` image, field by field in
//!   little-endian (the in-memory bytes on every host this engine runs on), padding as zeros —
//!   128 B per `PineSeqRec`, 64 B per `PineTapeRec`; no `unsafe` byte view.
//! - pin:08-set-tsv-columns: `set.tsv` is `<utc_ms>\t<enabled_mask hex>\t<matcher counters,
//!   comma-separated>`; the matcher's counter order is the caller's (`matcher_counters()`, paper
//!   arm only, E-2) and is stated once in the first line `#utc_ms\tmask\t<names>`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use strategy_core::{PineSeqRec, PineTapeRec, StrategyCounters};

/// Records drained per view call (the caller's buffers are this long).
pub const DRAIN_BATCH: usize = 256;

/// One `PineSeqRec` as its 128-byte image (pin:08-tape-bytes-le).
#[must_use]
pub fn seq_bytes(r: &PineSeqRec) -> [u8; 128] {
    let mut b = [0u8; 128];
    b[0..8].copy_from_slice(&r.seq.to_le_bytes());
    b[8..16].copy_from_slice(&r.pop_ns.to_le_bytes());
    b[16..24].copy_from_slice(&r.e_ns.to_le_bytes());
    b[24] = r.kind;
    // 25..64: padding (zeros).
    let mut k = 0usize;
    while k < 8 {
        b[64 + k * 8..72 + k * 8].copy_from_slice(&r.payload[k].to_le_bytes());
        k += 1;
    }
    b
}

/// One `PineTapeRec` as its 64-byte image (pin:08-tape-bytes-le).
#[must_use]
pub fn dec_bytes(r: &PineTapeRec) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0..8].copy_from_slice(&r.seq.to_le_bytes());
    b[8..16].copy_from_slice(&r.seq_ref.to_le_bytes());
    b[16..24].copy_from_slice(&r.x0.to_le_bytes());
    b[24..32].copy_from_slice(&r.x1.to_le_bytes());
    b[32..40].copy_from_slice(&r.x2.to_le_bytes());
    b[40..48].copy_from_slice(&r.aux.to_le_bytes());
    b[48..52].copy_from_slice(&r.bar_index.to_le_bytes());
    b[52..56].copy_from_slice(&r.oid.to_le_bytes());
    b[56] = r.kind;
    b[57] = r.script;
    b[58] = r.a;
    b[59] = r.b;
    b[60..64].copy_from_slice(&r.c.to_le_bytes());
    b
}

/// What one drain wrote and what it found missing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainOut {
    /// Sequencing records written.
    pub seq_written: u64,
    /// Decision records written.
    pub dec_written: u64,
    /// Sequencing records lost to an overrun (a `seq` gap): the window is void (I44).
    pub seq_gap: u64,
    /// Decision records lost to an overrun.
    pub dec_gap: u64,
}

/// The tape directory's three append-only files and the two read cursors.
#[derive(Debug)]
pub struct PineTapeWriter {
    dir: PathBuf,
    seq: File,
    dec: File,
    set: File,
    next_seq: u64,
    next_dec: u64,
    seq_buf: Box<[PineSeqRec]>,
    dec_buf: Box<[PineTapeRec]>,
}

impl PineTapeWriter {
    /// Create `<root>/<boot_utc_ms>/` with `seq.bin`, `decisions.bin` and `set.tsv` (the last
    /// starting with its header naming the matcher's counters). Boot-only.
    ///
    /// # Errors
    /// The directory or a file cannot be created.
    pub fn open(root: &Path, boot_utc_ms: i64, matcher_names: &[&str]) -> io::Result<Self> {
        let dir = root.join(boot_utc_ms.to_string());
        std::fs::create_dir_all(&dir)?;
        let open = |n: &str| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(n))
        };
        let seq = open("seq.bin")?;
        let dec = open("decisions.bin")?;
        let mut set = open("set.tsv")?;
        writeln!(set, "#utc_ms\tmask\t{}", matcher_names.join(","))?;
        Ok(Self {
            dir,
            seq,
            dec,
            set,
            next_seq: 1,
            next_dec: 0,
            seq_buf: vec![PineSeqRec::default(); DRAIN_BATCH].into_boxed_slice(),
            dec_buf: vec![PineTapeRec::default(); DRAIN_BATCH].into_boxed_slice(),
        })
    }

    /// The tape directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Drain both rings until empty; gaps are counted, never hidden (I44).
    ///
    /// # Errors
    /// A write failed.
    pub fn drain<S: StrategyCounters>(&mut self, strat: &S) -> io::Result<DrainOut> {
        let mut o = DrainOut::default();
        loop {
            let n = strat.pine_seq_view(&mut self.seq_buf, self.next_seq) as usize;
            if n == 0 {
                break;
            }
            for r in &self.seq_buf[..n] {
                o.seq_gap += r.seq.saturating_sub(self.next_seq);
                self.seq.write_all(&seq_bytes(r))?;
                self.next_seq = r.seq + 1;
                o.seq_written += 1;
            }
        }
        loop {
            let n = strat.pine_tape_view(&mut self.dec_buf, self.next_dec) as usize;
            if n == 0 {
                break;
            }
            for r in &self.dec_buf[..n] {
                o.dec_gap += r.seq.saturating_sub(self.next_dec);
                self.dec.write_all(&dec_bytes(r))?;
                self.next_dec = r.seq + 1;
                o.dec_written += 1;
            }
        }
        Ok(o)
    }

    /// One `set.tsv` line (pin:08-set-tsv-columns).
    ///
    /// # Errors
    /// A write failed.
    pub fn set_line(&mut self, utc_ms: i64, mask: u16, matcher: &[u64]) -> io::Result<()> {
        write!(self.set, "{utc_ms}\t{mask:#06x}\t")?;
        for (k, v) in matcher.iter().enumerate() {
            if k > 0 {
                self.set.write_all(b",")?;
            }
            write!(self.set, "{v}")?;
        }
        self.set.write_all(b"\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strategy_core::PineCounters;

    /// A member double holding `n_seq` sequencing and `n_dec` decision records, with the first
    /// `skip` sequencing seqs overwritten (a counted overrun).
    struct Rings {
        seq: Vec<PineSeqRec>,
        dec: Vec<PineTapeRec>,
    }

    impl StrategyCounters for Rings {
        fn strategy_kind(&self) -> &'static str {
            "pine"
        }
        fn pine_counters(&self) -> PineCounters {
            PineCounters::default()
        }
        fn pine_seq_view(&self, out: &mut [PineSeqRec], from: u64) -> u32 {
            let mut n = 0usize;
            for r in self.seq.iter().filter(|r| r.seq >= from) {
                if n == out.len() {
                    break;
                }
                out[n] = *r;
                n += 1;
            }
            n as u32
        }
        fn pine_tape_view(&self, out: &mut [PineTapeRec], from: u64) -> u32 {
            let mut n = 0usize;
            for r in self.dec.iter().filter(|r| r.seq >= from) {
                if n == out.len() {
                    break;
                }
                out[n] = *r;
                n += 1;
            }
            n as u32
        }
    }

    /// I44: the files hold exactly the records, in order, at their sizes; a seq gap is reported.
    #[test]
    fn drains_raw_records_and_reports_gaps() {
        let root = std::env::temp_dir().join(format!("pine-tape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut w =
            PineTapeWriter::open(&root, 1_767_225_600_000, &["judged", "filled"]).expect("open");
        let rings = Rings {
            // seqs 1..=600, then (overrun) 701..=800: 100 lost.
            seq: (1..=600u64)
                .chain(701..=800)
                .map(|s| PineSeqRec {
                    seq: s,
                    pop_ns: 7,
                    e_ns: s * 10,
                    kind: 1,
                    ..PineSeqRec::default()
                })
                .collect(),
            dec: (0..300u64)
                .map(|s| PineTapeRec {
                    seq: s,
                    seq_ref: s / 2,
                    kind: 5,
                    c: 3,
                    ..PineTapeRec::default()
                })
                .collect(),
        };
        let o = w.drain(&rings).expect("drain");
        assert_eq!(
            o,
            DrainOut {
                seq_written: 700,
                dec_written: 300,
                seq_gap: 100,
                dec_gap: 0
            }
        );
        assert_eq!(
            w.drain(&rings).expect("again"),
            DrainOut::default(),
            "idempotent"
        );
        w.set_line(1_767_225_605_000, 0x0100, &[3, 1]).expect("set");
        let seq = std::fs::read(w.dir().join("seq.bin")).expect("seq");
        let dec = std::fs::read(w.dir().join("decisions.bin")).expect("dec");
        assert_eq!((seq.len(), dec.len()), (700 * 128, 300 * 64));
        assert_eq!(&seq[0..8], &1u64.to_le_bytes());
        assert_eq!(&seq[128 * 600..128 * 600 + 8], &701u64.to_le_bytes());
        assert_eq!(&dec[64 * 299..64 * 299 + 8], &299u64.to_le_bytes());
        assert_eq!(dec[64 + 56], 5, "kind at byte 56");
        let set = std::fs::read_to_string(w.dir().join("set.tsv")).expect("set");
        assert_eq!(
            set,
            "#utc_ms\tmask\tjudged,filled\n1767225605000\t0x0100\t3,1\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The images match the PODs' `#[repr(C)]` sizes.
    #[test]
    fn record_images_are_pod_sized() {
        assert_eq!(
            core::mem::size_of::<PineSeqRec>(),
            seq_bytes(&PineSeqRec::default()).len()
        );
        assert_eq!(
            core::mem::size_of::<PineTapeRec>(),
            dec_bytes(&PineTapeRec::default()).len()
        );
    }
}
