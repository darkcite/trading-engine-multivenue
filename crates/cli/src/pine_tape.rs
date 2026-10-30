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
//! - pin:08-tape-buffered (risk r21 #3): each file is a `BufWriter` of [`TAPE_BUF_BYTES`]
//!   allocated at `open` (boot); a drain's records go to the buffer (one `write(2)` per full
//!   buffer, not per record) and [`PineTapeWriter::flush`] pushes all three files once per 5 s
//!   pass and at the final one. A write or flush error is returned (the caller logs it; the
//!   window is void).
//! - pin:08-tape-out-create-new (risk r21 #5): a replay tape (`backtest --replay-from-boot
//!   --tape-out`, [`PineTapeWriter::create_new`]) never appends — any of the three files (or
//!   `replay.tsv`) already under `<root>/<label>/` refuses it.
//! - pin:08-live-tape-create-new (risk r22; retires the live create-or-append): the live
//!   `--pine-tape` ([`PineTapeWriter::open`]) is created new as well — a second boot on the same
//!   `boot_utc_ms` label, or a clock stepped back onto an earlier one, finds the files and is
//!   refused (`AlreadyExists`), never appended into an earlier tape. The caller turns the tapes
//!   OFF for that boot with one ERROR line (the window is void); the boot itself goes on.
//!   "Exists" is `symlink_metadata` (a dangling link counts), and each file opens `create_new`.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use strategy_core::{PineSeqRec, PineTapeRec, StrategyCounters};

/// Records drained per view call (the caller's buffers are this long).
pub const DRAIN_BATCH: usize = 256;
/// Each tape file's write buffer (pin:08-tape-buffered).
pub const TAPE_BUF_BYTES: usize = 64 * 1024;
/// The tape directory's three files.
const TAPE_FILES: [&str; 3] = ["seq.bin", "decisions.bin", "set.tsv"];
/// pin:08-cross-replay-fills-file: a replay tape's fill-source record, next to the three files.
pub const REPLAY_FILE: &str = "replay.tsv";
/// pin:08-cross-replay-fills-file: its first line.
pub const REPLAY_MAGIC: &str = "# pvm-replay-1";

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
    seq: BufWriter<File>,
    dec: BufWriter<File>,
    set: BufWriter<File>,
    next_seq: u64,
    next_dec: u64,
    seq_buf: Box<[PineSeqRec]>,
    dec_buf: Box<[PineTapeRec]>,
}

impl PineTapeWriter {
    /// The live `--pine-tape`: create `<root>/<boot_utc_ms>/` with `seq.bin`, `decisions.bin`
    /// and `set.tsv` (the last starting with its header naming the matcher's counters), every
    /// file new (pin:08-live-tape-create-new). Boot-only.
    ///
    /// # Errors
    /// A tape file already exists under the label (`AlreadyExists`, nothing created or appended),
    /// or the directory or a file cannot be created.
    pub fn open(root: &Path, boot_utc_ms: i64, matcher_names: &[&str]) -> io::Result<Self> {
        Self::open_new(
            root,
            boot_utc_ms,
            matcher_names,
            "a second boot on this label or a clock stepped back",
        )
    }

    /// [`open`](Self::open) for a replay tape (pin:08-tape-out-create-new): every file is created
    /// new; an existing one under `<root>/<boot_utc_ms>/` is refused (`AlreadyExists`) before any
    /// file is created, so nothing is ever appended to an earlier tape.
    ///
    /// # Errors
    /// A tape file already exists, or the directory or a file cannot be created.
    pub fn create_new(root: &Path, boot_utc_ms: i64, matcher_names: &[&str]) -> io::Result<Self> {
        Self::open_new(
            root,
            boot_utc_ms,
            matcher_names,
            "remove it or choose another --tape-out",
        )
    }

    fn open_new(
        root: &Path,
        boot_utc_ms: i64,
        matcher_names: &[&str],
        hint: &str,
    ) -> io::Result<Self> {
        let dir = root.join(boot_utc_ms.to_string());
        if let Some(n) = TAPE_FILES
            .iter()
            .chain(core::iter::once(&REPLAY_FILE))
            .find(|n| std::fs::symlink_metadata(dir.join(n)).is_ok())
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists: a tape is never appended to ({hint})",
                    dir.join(n).display()
                ),
            ));
        }
        std::fs::create_dir_all(&dir)?;
        let open = |n: &str| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dir.join(n))
                .map(|f| BufWriter::with_capacity(TAPE_BUF_BYTES, f))
        };
        let seq = open(TAPE_FILES[0])?;
        let dec = open(TAPE_FILES[1])?;
        let mut set = open(TAPE_FILES[2])?;
        writeln!(set, "#utc_ms\tmask\t{}", matcher_names.join(","))?;
        // The header reaches the disk at boot, before the first 5 s pass.
        set.flush()?;
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

    /// pin:08-cross-replay-fills-file (risk r22 N1): a replay tape records its `--fills` source
    /// as `<dir>/replay.tsv` — `# pvm-replay-1`, then one `fills\t<recorded|judged>` line —
    /// created new (never overwritten). The live `--pine-tape` writes none. Boot-only.
    ///
    /// # Errors
    /// The file exists or cannot be written.
    pub fn replay_mode(&self, fills: &str) -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.dir.join(REPLAY_FILE))?;
        writeln!(f, "{REPLAY_MAGIC}\nfills\t{fills}")?;
        f.flush()
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

    /// Push the three buffers to their files (pin:08-tape-buffered): once per 5 s pass, after
    /// its `set.tsv` line, and at the final pass. Every file is flushed even when one fails.
    ///
    /// # Errors
    /// The first flush that failed (the window is void).
    pub fn flush(&mut self) -> io::Result<()> {
        let a = self.seq.flush();
        let b = self.dec.flush();
        let c = self.set.flush();
        a.and(b).and(c)
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
        // pin:08-tape-buffered: whole 64 KiB buffers only reach the file before the pass's flush.
        let early = std::fs::metadata(w.dir().join("seq.bin"))
            .expect("seq")
            .len();
        assert!(
            early < 700 * 128 && early % TAPE_BUF_BYTES as u64 == 0,
            "{early}"
        );
        w.flush().expect("flush");
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

    /// pin:08-tape-out-create-new (risk r21 #5): a replay tape refuses an existing file under
    /// its label and appends nothing; the live writer refuses it too
    /// (pin:08-live-tape-create-new).
    #[test]
    fn create_new_refuses_an_existing_tape() {
        let root = std::env::temp_dir().join(format!("pine-tape-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut w = PineTapeWriter::create_new(&root, 7, &["judged"]).expect("first");
        w.set_line(7, 0x0100, &[0]).expect("set");
        w.flush().expect("flush");
        drop(w);
        let set = root.join("7").join("set.tsv");
        let before = std::fs::read(&set).expect("set");
        let e = PineTapeWriter::create_new(&root, 7, &["judged"]).expect_err("refused");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(e.to_string().contains("never appended"), "{e}");
        assert_eq!(
            std::fs::read(&set).expect("set"),
            before,
            "nothing appended"
        );
        // Any one of the three files refuses it.
        std::fs::remove_file(root.join("7").join("seq.bin")).expect("rm");
        std::fs::remove_file(&set).expect("rm");
        assert!(PineTapeWriter::create_new(&root, 7, &["judged"]).is_err());
        assert!(
            !root.join("7").join("seq.bin").exists(),
            "refused before creating"
        );
        // pin:08-live-tape-create-new (risk r22): the live writer refuses it too.
        let e = PineTapeWriter::open(&root, 7, &["judged"]).expect_err("live refused");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(!root.join("7").join("seq.bin").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// pin:08-live-tape-create-new (risk r22): a second live boot on the same label (or a clock
    /// stepped back onto it) is refused before any file is created — the first boot's tape keeps
    /// its bytes; a dangling link under the label counts as a file; a fresh label opens.
    #[test]
    fn live_open_refuses_a_second_boot_on_the_same_label() {
        let root = std::env::temp_dir().join(format!("pine-tape-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut w = PineTapeWriter::open(&root, 9, &["judged"]).expect("first boot");
        w.set_line(9, 0x0100, &[1]).expect("set");
        w.flush().expect("flush");
        drop(w);
        let set = root.join("9").join("set.tsv");
        let before = std::fs::read(&set).expect("set");
        let e = PineTapeWriter::open(&root, 9, &["judged"]).expect_err("second boot");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(e.to_string().contains("never appended"), "{e}");
        assert!(e.to_string().contains("clock stepped back"), "{e}");
        assert_eq!(
            std::fs::read(&set).expect("set"),
            before,
            "nothing appended"
        );
        std::fs::create_dir_all(root.join("10")).expect("dir");
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("10").join("decisions.bin"))
            .expect("link");
        assert!(PineTapeWriter::open(&root, 10, &["judged"]).is_err());
        assert!(!root.join("nowhere").exists(), "never written through");
        assert!(PineTapeWriter::open(&root, 11, &["judged"]).is_ok());
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
