// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # PMLR reader
//!
//! Zero-copy, zero-alloc reader for the replay-log format produced by
//! [`crate::PmlrWriter`]. Uses `mmap(PROT_READ, MAP_PRIVATE)` to expose
//! the file as a byte slice and then transmutes the post-header tail
//! into `&[R]` for any `R: core_types::AsBytes + 'static`.
//!
//! ## Why raw `libc::mmap`
//!
//! We already depend on `libc` for the write-side. Pulling in
//! `memmap2` would grow the dependency tree for 20 lines of mapping
//! code. The safe wrapper here upholds all the invariants (`ptr.read`,
//! alignment, lifetimes) the `memmap2::Mmap` type would otherwise
//! enforce.
//!
//! ## Layout
//!
//! ```text
//!     ┌──── 64 B header ────┐┌── 64 B slot ──┐┌── 64 B slot ──┐  ...
//!     │ b"PMLR" | v | kind │ │  POD record  │ │  POD record  │
//!     └─────────────────────┘└──────────────┘└──────────────┘
//! ```
//!
//! ## Zero-alloc
//!
//! [`PmlrReader::open`] allocates nothing on the Rust heap. The only
//! resource it owns is a kernel-side memory mapping plus its backing
//! file descriptor, both released in `Drop`.

use core::marker::PhantomData;
use std::io;
use std::path::Path;

use core_types::AsBytes;

use crate::pmlr::{HEADER_SIZE, MAGIC, SLOT_SIZE, VERSION};
use crate::SlotKind;

// ---------------------------------------------------------------
// Error / result types
// ---------------------------------------------------------------

/// Reasons a PMLR file can fail the reader's validity checks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmlrReadErr {
    /// File is shorter than the 64-byte header.
    TruncatedHeader,
    /// Magic bytes don't match `b"PMLR"`.
    BadMagic,
    /// Version in header is newer than the reader supports.
    UnsupportedVersion(u16),
    /// `slot_kind` byte is not a valid [`SlotKind`] variant.
    UnknownSlotKind(u8),
    /// Payload length after the header isn't a multiple of [`SLOT_SIZE`].
    PayloadNotMultipleOfSlot,
    /// Caller-typed record size doesn't equal [`SLOT_SIZE`].
    RecordSizeMismatch(usize),
}

impl core::fmt::Display for PmlrReadErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TruncatedHeader => write!(f, "PMLR file truncated before header"),
            Self::BadMagic => write!(f, "PMLR file has wrong magic bytes"),
            Self::UnsupportedVersion(v) => write!(f, "PMLR version {v} unsupported"),
            Self::UnknownSlotKind(b) => write!(f, "PMLR slot_kind byte {b} unknown"),
            Self::PayloadNotMultipleOfSlot => write!(f, "PMLR payload not a multiple of 64 B"),
            Self::RecordSizeMismatch(n) => write!(f, "record size {n} != 64"),
        }
    }
}

impl std::error::Error for PmlrReadErr {}

// ---------------------------------------------------------------
// Reader
// ---------------------------------------------------------------

/// mmap-backed reader over a PMLR file. Parameterised by the record
/// POD `R` — `open` returns `Err` if the file's declared slot kind does
/// not match `R`'s size (64 B).
///
/// ## Safety invariants upheld internally
///
/// 1. `ptr` / `len` come from a successful `mmap(PROT_READ,
///    MAP_PRIVATE)` and stay live for the reader's entire lifetime.
/// 2. The slice `records()` returns has length
///    `(len - HEADER_SIZE) / SLOT_SIZE` and is aligned to at least
///    `align_of::<R>()` because `mmap` always returns page-aligned
///    memory (4 KiB on Unix; page size >= max cache-line alignment
///    used by our POD types).
/// 3. `R: AsBytes` is an unsafe marker trait implemented only for
///    `#[repr(C)] + Copy` types with no uninitialised padding.
pub struct PmlrReader<R: AsBytes> {
    ptr: *const u8,
    len: usize,
    fd: libc::c_int,
    slot_kind: SlotKind,
    epoch_ns: u64,
    version: u16,
    record_count: usize,
    _marker: PhantomData<R>,
}

impl<R: AsBytes> PmlrReader<R> {
    /// Open `path`, mmap its contents, and validate the header.
    ///
    /// # Errors
    ///
    /// Any underlying `open`/`fstat`/`mmap` syscall failure is surfaced
    /// as `io::Error`. Header-validation errors surface as
    /// `io::Error::other(PmlrReadErr)`.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        if core::mem::size_of::<R>() != SLOT_SIZE {
            return Err(io::Error::other(PmlrReadErr::RecordSizeMismatch(
                core::mem::size_of::<R>(),
            )));
        }

        let path = path.as_ref();
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        // SAFETY: c_path outlives the call; O_RDONLY is a valid flag.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // fstat to determine length.
        // SAFETY: `stat` is an out-parameter the kernel fills.
        let mut st: libc::stat = unsafe { core::mem::zeroed() };
        // SAFETY: fd is valid; &mut st is valid.
        let rc = unsafe { libc::fstat(fd, &mut st) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            // SAFETY: fd owned by us, unused after this point.
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }
        let len = st.st_size as usize;

        if len < HEADER_SIZE {
            // SAFETY: fd is valid and owned by us.
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::other(PmlrReadErr::TruncatedHeader));
        }

        // mmap read-only, MAP_PRIVATE (copy-on-write but we never write).
        // SAFETY: `len > 0` (guarded above); `fd` is valid; NULL tells
        // the kernel to pick an address.
        let raw = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            let err = io::Error::last_os_error();
            // SAFETY: fd valid.
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }
        let ptr = raw as *const u8;

        // Validate header.
        // SAFETY: ptr was just returned by mmap with len >= HEADER_SIZE.
        let header = unsafe { core::slice::from_raw_parts(ptr, HEADER_SIZE) };
        if header[0..4] != MAGIC {
            // SAFETY: ptr + len valid; fd valid.
            unsafe {
                libc::munmap(raw, len);
                libc::close(fd);
            }
            return Err(io::Error::other(PmlrReadErr::BadMagic));
        }
        let ver = u16::from_le_bytes([header[4], header[5]]);
        if ver > VERSION {
            // SAFETY: ptr + len + fd valid.
            unsafe {
                libc::munmap(raw, len);
                libc::close(fd);
            }
            return Err(io::Error::other(PmlrReadErr::UnsupportedVersion(ver)));
        }
        let slot_kind = match SlotKind::from_u8(header[6]) {
            Some(k) => k,
            None => {
                // SAFETY: ptr + len + fd valid.
                unsafe {
                    libc::munmap(raw, len);
                    libc::close(fd);
                }
                return Err(io::Error::other(PmlrReadErr::UnknownSlotKind(header[6])));
            }
        };
        let epoch_ns = u64::from_le_bytes([
            header[8], header[9], header[10], header[11], header[12], header[13], header[14],
            header[15],
        ]);

        let payload_len = len - HEADER_SIZE;
        if payload_len % SLOT_SIZE != 0 {
            // SAFETY: ptr + len + fd valid.
            unsafe {
                libc::munmap(raw, len);
                libc::close(fd);
            }
            return Err(io::Error::other(PmlrReadErr::PayloadNotMultipleOfSlot));
        }

        Ok(Self {
            ptr,
            len,
            fd,
            version: ver,
            slot_kind,
            epoch_ns,
            record_count: payload_len / SLOT_SIZE,
            _marker: PhantomData,
        })
    }

    /// Slot kind declared in the header.
    #[inline]
    pub fn slot_kind(&self) -> SlotKind {
        self.slot_kind
    }

    /// PMLR header version of the opened file. v1 files are readable
    /// but venue-less (`Tick.venue`/`Order.venue` bytes fall inside
    /// what v1 wrote as undefined implicit padding — consumers must
    /// treat them as garbage when `version() == 1`).
    #[inline]
    pub fn version(&self) -> u16 {
        self.version
    }

    /// Wall-clock epoch (ns) written at file creation.
    #[inline]
    pub fn epoch_ns(&self) -> u64 {
        self.epoch_ns
    }

    /// Number of records after the header.
    #[inline]
    #[allow(clippy::misnamed_getters)]
    pub fn len(&self) -> usize {
        self.record_count
    }

    /// Whether the file has zero records.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.record_count == 0
    }

    /// Zero-copy view over the records as a `&[R]`. The slice borrows
    /// the mmap for the reader's lifetime — no allocation.
    #[inline]
    pub fn records(&self) -> &[R] {
        // SAFETY:
        // * `ptr.add(HEADER_SIZE)` is within the mapping (we verified
        //   `len >= HEADER_SIZE`).
        // * `record_count * SLOT_SIZE + HEADER_SIZE == len` (checked
        //   in `open`).
        // * `R: AsBytes` ⇒ `R: Copy` + `#[repr(C)]` with no
        //   uninit padding, so the bytes are a valid `R` bit-pattern
        //   (the writer produced them from a live `R`).
        // * `mmap` returns page-aligned memory; every POD with `AsBytes`
        //   in this workspace is `#[repr(align(64))]`, which divides
        //   page size on every target we support.
        unsafe {
            core::slice::from_raw_parts(self.ptr.add(HEADER_SIZE).cast::<R>(), self.record_count)
        }
    }

    /// Fetch a single record by index, bounds-checked. Returns `None`
    /// if `i >= len()`.
    #[inline]
    pub fn get(&self, i: usize) -> Option<R> {
        if i < self.record_count {
            // SAFETY: bounds checked above; alignment + repr guaranteed
            // by `AsBytes`.
            Some(unsafe { self.ptr.add(HEADER_SIZE + i * SLOT_SIZE).cast::<R>().read() })
        } else {
            None
        }
    }
}

impl<R: AsBytes> core::fmt::Debug for PmlrReader<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PmlrReader")
            .field("slot_kind", &self.slot_kind)
            .field("epoch_ns", &self.epoch_ns)
            .field("record_count", &self.record_count)
            .field("len", &self.len)
            .finish()
    }
}

impl<R: AsBytes> Drop for PmlrReader<R> {
    fn drop(&mut self) {
        // SAFETY: ptr / len / fd all came from a successful open() call
        // and have not been released yet.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
            libc::close(self.fd);
        }
    }
}

// The raw pointer is safe to send / share because the underlying
// mapping is read-only and never mutated after open().
// SAFETY: Nothing mutates `ptr`/`len`/`fd` after construction; `R`
// is `Copy`. The kernel mapping is read-only.
unsafe impl<R: AsBytes + Send> Send for PmlrReader<R> {}
// SAFETY: See `Send` impl — reads through a shared reference only hit
// immutable mmap bytes.
unsafe impl<R: AsBytes + Sync> Sync for PmlrReader<R> {}

// ---------------------------------------------------------------
// Tests — roundtrip against PmlrWriter
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PmlrWriter;
    use core_types::{LatencyClass, NsTs, Price, Qty, Signal, SignalSource, Tick, VenueId};

    fn unique_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        dir.join(format!(
            "core_io_reader_{tag}_{}_{}.pmlr",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn reader_roundtrips_ticks() {
        let p = unique_path("ticks");
        let _ = std::fs::remove_file(&p);

        let mut w = PmlrWriter::open(&p, SlotKind::Tick, 0xDEADBEEF).unwrap();
        let t1 = Tick::new(
            1_000 as NsTs,
            VenueId::Polymarket,
            7,
            42,
            Price::from_raw(500_000),
            Qty::from_raw(100),
            Price::from_raw(510_000),
            Qty::from_raw(50),
        );
        let t2 = Tick::new(
            2_000 as NsTs,
            VenueId::Polymarket,
            7,
            43,
            Price::from_raw(505_000),
            Qty::from_raw(120),
            Price::from_raw(515_000),
            Qty::from_raw(60),
        );
        w.append(&t1).unwrap();
        w.append(&t2).unwrap();
        w.flush().unwrap();
        drop(w);

        let r = PmlrReader::<Tick>::open(&p).unwrap();
        assert_eq!(r.slot_kind(), SlotKind::Tick);
        assert_eq!(r.epoch_ns(), 0xDEADBEEF);
        assert_eq!(r.len(), 2);
        assert!(!r.is_empty());
        let got = r.records();
        assert_eq!(got[0].sym, 7);
        assert_eq!(got[0].venue_seq, 42);
        assert_eq!(got[0].bid_px.raw(), 500_000);
        assert_eq!(got[1].venue_seq, 43);
        assert_eq!(got[1].ask_qty.raw(), 60);

        let one = r.get(1).unwrap();
        assert_eq!(one.venue_seq, 43);
        assert!(r.get(2).is_none());
        drop(r);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reader_roundtrips_signals() {
        let p = unique_path("signals");
        let _ = std::fs::remove_file(&p);

        let mut w = PmlrWriter::open(&p, SlotKind::Signal, 1).unwrap();
        let payload = {
            let mut x = [0u8; 40];
            x[0] = 1;
            x[39] = 99;
            x
        };
        let s = Signal::new(
            77 as NsTs,
            11,
            LatencyClass::Warm,
            SignalSource::Rpc as u8,
            payload,
        );
        w.append(&s).unwrap();
        w.flush().unwrap();
        drop(w);

        let r = PmlrReader::<Signal>::open(&p).unwrap();
        assert_eq!(r.slot_kind(), SlotKind::Signal);
        assert_eq!(r.len(), 1);
        let got = r.records();
        assert_eq!(got[0].sym, 11);
        assert!(matches!(got[0].class, LatencyClass::Warm));
        assert_eq!(got[0].payload[0], 1);
        assert_eq!(got[0].payload[39], 99);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reader_roundtrips_ai_cmds() {
        // 8f: SlotKind::AiCmd = 4 decodes end-to-end — write two
        // shape-valid commands, mmap them back, revalidate.
        let p = unique_path("aicmds");
        let _ = std::fs::remove_file(&p);

        let mut w = PmlrWriter::open(&p, SlotKind::AiCmd, 7).unwrap();
        let hb = core_types::AiCmd::new(
            1_000,
            1,
            core_types::SYMBOL_ID_NONE,
            0,
            0,
            0,
            core_types::AiCmdKind::Heartbeat,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_NONE,
            core_types::AI_SIDE_NONE,
            0,
            0,
        );
        let fv = core_types::AiCmd::new(
            2_000,
            2,
            core_types::make_symbol_id(VenueId::Polymarket, 9),
            750_000,
            0,
            5_000_000_000,
            core_types::AiCmdKind::SetFairValue,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_NONE,
            core_types::AI_SIDE_NONE,
            0,
            core_types::AI_CMD_FLAG_EXPIRE_ON_SILENCE,
        );
        w.append(&hb).unwrap();
        w.append(&fv).unwrap();
        w.flush().unwrap();
        drop(w);

        let r = PmlrReader::<core_types::AiCmd>::open(&p).unwrap();
        assert_eq!(r.slot_kind(), SlotKind::AiCmd);
        assert_eq!(r.epoch_ns(), 7);
        assert_eq!(r.len(), 2);
        let got = r.records();
        assert_eq!(got[0].kind(), Some(core_types::AiCmdKind::Heartbeat));
        assert_eq!(got[0].seq, 1);
        assert_eq!(got[0].validate_shape(), Ok(()));
        assert_eq!(got[1].kind(), Some(core_types::AiCmdKind::SetFairValue));
        assert_eq!(got[1].px, 750_000);
        assert_eq!(got[1].ttl_ns, 5_000_000_000);
        assert_eq!(got[1].flags, core_types::AI_CMD_FLAG_EXPIRE_ON_SILENCE);
        assert_eq!(got[1].validate_shape(), Ok(()));
        drop(r);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reader_rejects_bad_magic() {
        let p = unique_path("badmagic");
        let _ = std::fs::remove_file(&p);
        // Craft a file with 64 bytes of zero — magic check fails.
        std::fs::write(&p, [0u8; 64]).unwrap();
        let err = PmlrReader::<Tick>::open(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reader_rejects_truncated_header() {
        let p = unique_path("short");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, [0u8; 16]).unwrap();
        let err = PmlrReader::<Tick>::open(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reader_rejects_non_multiple_payload() {
        let p = unique_path("ragged");
        let _ = std::fs::remove_file(&p);
        // Write a valid header followed by 5 bytes of garbage.
        let mut out = vec![0u8; 64 + 5];
        out[0..4].copy_from_slice(b"PMLR");
        out[4..6].copy_from_slice(&1u16.to_le_bytes());
        out[6] = SlotKind::Tick.to_u8();
        std::fs::write(&p, &out).unwrap();
        let err = PmlrReader::<Tick>::open(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reader_handles_empty_log() {
        let p = unique_path("empty");
        let _ = std::fs::remove_file(&p);
        let w = PmlrWriter::open(&p, SlotKind::Fill, 0).unwrap();
        drop(w);
        let r = PmlrReader::<core_types::Fill>::open(&p).unwrap();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        let _ = std::fs::remove_file(&p);
    }
}
