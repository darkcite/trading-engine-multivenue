// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-io
//!
//! Non-hot-path file I/O. The replay log writer (pwrite-based append)
//! lives here. Kept out of the hot path because `pwrite` is a syscall
//! and we prefer to amortise it by buffering a whole batch before
//! issuing it.
//!
//! ## Scaffold status
//!
//! Phase 0 ships the shape: a `PreallocatedWriter` type that owns a
//! file descriptor and a fixed 64 KiB staging buffer. Real replay-log
//! framing arrives in Phase 1 alongside `docs/wire-format.md`.

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
pub mod pmlr;
pub mod pmlr_reader;
pub mod slot_capture;
pub use capture::{
    PmlrCapture, RawTapReader, RawTapRecord, TapCfg, TapMode, CAPTURE_FLUSH_INTERVAL_NS,
    DEFAULT_TAP_BUDGET_BYTES, RAW_TAP_FLAG_REJECT,
};
pub use pmlr::{PmlrWriter, SlotKind, DEFAULT_STAGING_SIZE, HEADER_SIZE, MAGIC, SLOT_SIZE, VERSION};
pub use pmlr_reader::{PmlrReadErr, PmlrReader};
pub use slot_capture::SlotCapture;

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// Append-only writer with a preallocated staging buffer. Flushed
/// explicitly by the caller; never flushed implicitly on drop.
pub struct PreallocatedWriter {
    file: File,
    buf: Box<[u8]>,
    len: usize,
}

impl PreallocatedWriter {
    /// Open `path` for append + create, allocate a fixed staging buffer.
    pub fn open<P: AsRef<Path>>(path: P, buf_size: usize) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file,
            buf: vec![0u8; buf_size].into_boxed_slice(),
            len: 0,
        })
    }

    /// Bytes currently staged.
    #[inline]
    pub fn staged(&self) -> usize {
        self.len
    }

    /// Push one byte into the staging buffer, flushing first if full.
    #[inline]
    pub fn push_byte(&mut self, b: u8) -> io::Result<()> {
        if self.len == self.buf.len() {
            self.flush_staging()?;
        }
        self.buf[self.len] = b;
        self.len += 1;
        Ok(())
    }

    /// Push a slice, flushing segments as needed.
    pub fn push_slice(&mut self, s: &[u8]) -> io::Result<()> {
        let mut i = 0;
        while i < s.len() {
            let room = self.buf.len() - self.len;
            let n = room.min(s.len() - i);
            self.buf[self.len..self.len + n].copy_from_slice(&s[i..i + n]);
            self.len += n;
            i += n;
            if self.len == self.buf.len() {
                self.flush_staging()?;
            }
        }
        Ok(())
    }

    /// Drain the staging buffer to disk.
    pub fn flush_staging(&mut self) -> io::Result<()> {
        if self.len > 0 {
            self.file.write_all(&self.buf[..self.len])?;
            self.len = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn push_slice_roundtrips_through_file() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("core_io_test_{}.log", std::process::id()));
        // Ensure clean slate.
        let _ = std::fs::remove_file(&p);
        let mut w = PreallocatedWriter::open(&p, 8).expect("open");
        w.push_slice(b"hello ").unwrap();
        w.push_slice(b"world").unwrap();
        w.flush_staging().unwrap();
        drop(w);

        let mut got = String::new();
        File::open(&p).unwrap().read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello world");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn staged_reports_zero_after_flush() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("core_io_test_flush_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut w = PreallocatedWriter::open(&p, 16).unwrap();
        w.push_slice(b"abc").unwrap();
        assert_eq!(w.staged(), 3);
        w.flush_staging().unwrap();
        assert_eq!(w.staged(), 0);
        let _ = std::fs::remove_file(&p);
    }
}
