// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Bar files (contract §3.16.3): the 64-byte header, row validation (I4), the read-only mapping
//! (`open`, CAR-07-1), the clamped owned read (`open_clamped`, I8), the streaming digest check and
//! the preparer's pure half (`build`, re-exported here).

mod build;

pub use build::{
    bars_from_candles, bars_from_trades, dec_to_1e6, tiles, write_bar_file, BuildStats, CandleRow,
    TradePrint, WriteOutcome,
};

use crate::codec::row_bytes;
use core_crypto::Sha256;
use core_time::barclock;
use pine_ir::{barflag, Bar, DiagCode};
use std::path::{Path, PathBuf};

/// `b"PVMBARS1"`.
pub const BARFILE_MAGIC: [u8; 8] = *b"PVMBARS1";
/// Format version.
pub const BARFILE_VERSION: u32 = 1;
/// File extension: `<root>/<venue>/<symbol>/<tf_s>.bars`.
pub const BARFILE_EXT: &str = "bars";

/// [`BarFileHdr::source`] values.
pub mod bar_source {
    /// The venue's own candles (`candles.db`, fetched or worker-derived from fetched bases).
    pub const CANDLES: u8 = 1;
    /// Captured trade prints (the B'1 payload, contract §3.15.2), bucketed by venue time.
    pub const TRADES: u8 = 2;
}

/// The 64-byte header of a bar file, followed by `n` × `Bar` (contract §3.16.3). Little-endian on
/// disk; the reader decodes it field by field (the codec is the cold `codec` module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct BarFileHdr {
    /// [`BARFILE_MAGIC`].
    pub magic: [u8; 8],
    /// [`BARFILE_VERSION`].
    pub version: u32,
    /// Bar length, seconds; `core_time::barclock::is_allowed`.
    pub tf_s: u32,
    /// `pine_ir::symbol_hash` of the canonical `venue:symbol` key.
    pub symbol_hash: u64,
    /// `open_ms` of row 0.
    pub first_open_ms: i64,
    /// `open_ms` of row `n − 1`.
    pub last_open_ms: i64,
    /// Row count, ≥ 1.
    pub n: u64,
    /// Buckets absent between the first and the last row (`Σ bars_between − 1`).
    pub holes: u32,
    /// `core_types::VenueId` as a raw byte (the host fills it; this crate never names `core-types`).
    pub venue: u8,
    /// [`bar_source`] value.
    pub source: u8,
    /// Always zero.
    pub _pad: [u8; 2],
    /// First 8 bytes (LE) of the SHA-256 of the `n × 64` row bytes — the source digest.
    pub src_digest64: u64,
}
const _: () = assert!(core::mem::size_of::<BarFileHdr>() == 64);
const _: () = assert!(core::mem::align_of::<BarFileHdr>() == 8);
const _: () = assert!(core::mem::offset_of!(BarFileHdr, n) == 40);
const _: () = assert!(core::mem::offset_of!(BarFileHdr, src_digest64) == 56);
const _: () = assert!(cfg!(target_endian = "little")); // rows are mapped as `Bar` in place

/// The rows a clamped open keeps: `open_ms ≥ from_open_ms` and `close_ms ≤ to_close_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clamp {
    /// Earliest bar open kept.
    pub from_open_ms: i64,
    /// Latest bar close kept.
    pub to_close_ms: i64,
}

/// Why a bar file (or a preparer input) was refused. Every variant is `DiagCode(7001)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarFileErr {
    /// An I/O error (`raw_os_error`, or −1).
    Io(i32),
    /// The source database is 0 bytes (the 2026-09-12 stray).
    ZeroByteSource,
    /// Magic is not `PVMBARS1`.
    BadMagic,
    /// Version is not 1.
    BadVersion(u32),
    /// A timeframe outside `barclock::ALLOWED_TF_S`.
    BadTimeframe(u32),
    /// Not a canonical `venue:symbol` key.
    BadSymbolKey,
    /// File length ≠ `64 + 64·n`.
    Truncated {
        /// Actual length in bytes.
        len: u64,
    },
    /// No row (or none inside the clamp, or none after aggregation).
    Empty,
    /// `open_ms` not strictly ascending.
    NotAscending {
        /// Offending row.
        row: u64,
    },
    /// `open_ms` not on the barclock grid.
    BoundaryMismatch {
        /// Offending row.
        row: u64,
    },
    /// `close_ms ≠ barclock::close_ms(open_ms, tf_s)`.
    CloseMismatch {
        /// Offending row.
        row: u64,
    },
    /// `index ≠ row`.
    IndexMismatch {
        /// Offending row.
        row: u64,
    },
    /// A flag outside `CONFIRMED | GAP_BEFORE | VOL_SAT`, or `CONFIRMED` clear.
    BadFlags {
        /// Offending row.
        row: u64,
    },
    /// `GAP_BEFORE` disagrees with the hole before the row.
    GapFlag {
        /// Offending row.
        row: u64,
    },
    /// A price ≤ 0 or `low ≤ min(open, close) ≤ max(open, close) ≤ high` violated.
    BadPrice {
        /// Offending row.
        row: u64,
    },
    /// Volume < 0, or `VOL_SAT` disagrees with `volume_1e6 == i64::MAX`.
    BadVolume {
        /// Offending row.
        row: u64,
    },
    /// `first/last_open_ms`, `holes`, `source` or `_pad` disagree with the rows.
    HeaderMismatch,
    /// `src_digest64` disagrees with the rows.
    DigestMismatch,
    /// Aggregation from a source timeframe that does not tile the target.
    NotADivisor {
        /// Source bar length.
        src_tf_s: u32,
        /// Target bar length.
        tf_s: u32,
    },
    /// A trade print outside every coverage interval.
    OutsideCoverage {
        /// Offending print.
        row: u64,
    },
}

impl BarFileErr {
    /// Always `DiagCode(7001)` (contract §3.3: "bar file refused").
    pub const fn code(&self) -> DiagCode {
        DiagCode(7001)
    }

    /// A fixed English reason (for `RunError::BarFile { reason }` and the CLI).
    pub const fn why(&self) -> &'static str {
        match self {
            BarFileErr::Io(_) => "i/o error",
            BarFileErr::ZeroByteSource => "zero-byte source",
            BarFileErr::BadMagic => "bad magic",
            BarFileErr::BadVersion(_) => "bad version",
            BarFileErr::BadTimeframe(_) => "timeframe not allowed",
            BarFileErr::BadSymbolKey => "not a canonical symbol key",
            BarFileErr::Truncated { .. } => "truncated",
            BarFileErr::Empty => "no rows",
            BarFileErr::NotAscending { .. } => "open_ms not ascending",
            BarFileErr::BoundaryMismatch { .. } => "open_ms off the barclock grid",
            BarFileErr::CloseMismatch { .. } => "close_ms is not the barclock close",
            BarFileErr::IndexMismatch { .. } => "index is not the row",
            BarFileErr::BadFlags { .. } => "bad flags",
            BarFileErr::GapFlag { .. } => "GAP_BEFORE disagrees with the hole",
            BarFileErr::BadPrice { .. } => "bad price",
            BarFileErr::BadVolume { .. } => "bad volume",
            BarFileErr::HeaderMismatch => "header disagrees with the rows",
            BarFileErr::DigestMismatch => "digest mismatch",
            BarFileErr::NotADivisor { .. } => "source timeframe does not tile the target",
            BarFileErr::OutsideCoverage { .. } => "trade print outside coverage",
        }
    }

    const fn row(&self) -> Option<u64> {
        match *self {
            BarFileErr::NotAscending { row }
            | BarFileErr::BoundaryMismatch { row }
            | BarFileErr::CloseMismatch { row }
            | BarFileErr::IndexMismatch { row }
            | BarFileErr::BadFlags { row }
            | BarFileErr::GapFlag { row }
            | BarFileErr::BadPrice { row }
            | BarFileErr::BadVolume { row }
            | BarFileErr::OutsideCoverage { row } => Some(row),
            _ => None,
        }
    }
}

impl core::fmt::Display for BarFileErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} {}", self.code().0, self.why())?;
        if let Some(r) = self.row() {
            write!(f, " row {r}")?;
        }
        Ok(())
    }
}

/// `<root>/<venue>/<symbol>/<tf_s>.bars` for a canonical key; a `/` inside the symbol is written
/// `%2f` (the only character of the canonical alphabet that cannot be a path component).
pub fn bar_path(root: &Path, symbol_key: &str, tf_s: u32) -> Result<PathBuf, BarFileErr> {
    if !pine_ir::is_canonical_symbol_key(symbol_key.as_bytes()) {
        return Err(BarFileErr::BadSymbolKey);
    }
    if !barclock::is_allowed(tf_s) {
        return Err(BarFileErr::BadTimeframe(tf_s));
    }
    let (venue, symbol) = match symbol_key.split_once(':') {
        Some(p) => p,
        None => return Err(BarFileErr::BadSymbolKey),
    };
    let mut p = root.join(venue);
    p.push(symbol.replace('/', "%2f"));
    p.push(format!("{tf_s}.{BARFILE_EXT}"));
    Ok(p)
}

/// A bar file's rows, read-only. `rows()[i].index == i` for every `i` (I4, I8).
pub struct BarFile {
    hdr: BarFileHdr,
    backing: Backing,
    clamp: Option<Clamp>,
}

/// Where the rows live: the read-only mapping of the whole file, or memory this value owns.
enum Backing {
    /// `mmap(PROT_READ, MAP_PRIVATE)` of the whole file; rows start at `ptr + 64`.
    #[cfg(unix)]
    Map {
        ptr: *const u8,
        len: usize,
        n: usize,
    },
    /// Owned rows (`from_rows`, `open_clamped`, and `open` off unix).
    Owned(Vec<Bar>),
}

// SAFETY: the mapping is read-only, never written after `open`, and `Bar` is plain integers.
unsafe impl Send for BarFile {}
// SAFETY: as above; shared `&[Bar]` reads only.
unsafe impl Sync for BarFile {}

/// Offset of `Bar::open_ms` inside a row (the only field a clamped open's search reads).
const ROW_OPEN_MS_OFF: u64 = 40;
/// Header and row size, bytes.
const REC: u64 = 64;

impl BarFileHdr {
    /// The header the writer stamps on `rows` (already validated, `holes` their hole count).
    pub(crate) fn of_rows(
        rows: &[Bar],
        holes: u32,
        tf_s: u32,
        symbol_hash: u64,
        venue: u8,
        source: u8,
    ) -> BarFileHdr {
        let n = rows.len();
        BarFileHdr {
            magic: BARFILE_MAGIC,
            version: BARFILE_VERSION,
            tf_s,
            symbol_hash,
            first_open_ms: rows[0].open_ms,
            last_open_ms: rows[n - 1].open_ms,
            n: n as u64,
            holes,
            venue,
            source,
            _pad: [0; 2],
            src_digest64: src_digest64(rows),
        }
    }
}

/// First 8 bytes (LE) of the SHA-256 of the rows' on-disk bytes.
fn src_digest64(rows: &[Bar]) -> u64 {
    let mut h = Sha256::new();
    let mut i = 0;
    while i < rows.len() {
        h.update(&row_bytes(&rows[i]));
        i += 1;
    }
    let d = h.finalize();
    u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// The checks a header passes before any row is looked at: magic, version, timeframe, `n ≥ 1`,
/// and `len == 64 + 64·n`.
fn check_hdr_shape(h: &BarFileHdr, len: u64) -> Result<(), BarFileErr> {
    if h.magic != BARFILE_MAGIC {
        return Err(BarFileErr::BadMagic);
    }
    if h.version != BARFILE_VERSION {
        return Err(BarFileErr::BadVersion(h.version));
    }
    if !barclock::is_allowed(h.tf_s) {
        return Err(BarFileErr::BadTimeframe(h.tf_s));
    }
    if h.n == 0 {
        return Err(BarFileErr::Empty);
    }
    match h.n.checked_mul(REC).and_then(|b| b.checked_add(REC)) {
        Some(want) if want == len => Ok(()),
        _ => Err(BarFileErr::Truncated { len }),
    }
}

/// The header fields that must agree with validated rows (I4's last bullet).
// pin:07-source-check: "`source` … disagree with the rows" — a row carries no source, so the
// literal check is that `source` is a defined `bar_source` value.
fn check_hdr_rows(h: &BarFileHdr, rows: &[Bar], holes: u32) -> Result<(), BarFileErr> {
    let n = rows.len();
    if h.first_open_ms != rows[0].open_ms
        || h.last_open_ms != rows[n - 1].open_ms
        || h.holes != holes
        || !(h.source == bar_source::CANDLES || h.source == bar_source::TRADES)
        || h._pad != [0, 0]
    {
        return Err(BarFileErr::HeaderMismatch);
    }
    Ok(())
}

fn io_err(e: &std::io::Error) -> BarFileErr {
    BarFileErr::Io(e.raw_os_error().unwrap_or(-1))
}

/// `pread(2)` exactly `buf.len()` bytes at `off`.
fn pread_exact(f: &std::fs::File, buf: &mut [u8], off: u64) -> Result<(), BarFileErr> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.read_exact_at(buf, off).map_err(|e| io_err(&e))
    }
    #[cfg(not(unix))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut g = f;
        g.seek(SeekFrom::Start(off)).map_err(|e| io_err(&e))?;
        g.read_exact(buf).map_err(|e| io_err(&e))
    }
}

/// Open `path` and read its header; returns the file, its length and the decoded header.
fn open_hdr(path: &Path) -> Result<(std::fs::File, u64, BarFileHdr), BarFileErr> {
    let f = std::fs::File::open(path).map_err(|e| io_err(&e))?;
    let len = f.metadata().map_err(|e| io_err(&e))?.len();
    if len < REC {
        return Err(BarFileErr::Truncated { len });
    }
    let mut hb = [0u8; 64];
    pread_exact(&f, &mut hb, 0)?;
    Ok((f, len, BarFileHdr::from_le_bytes(&hb)))
}

/// The `n` rows of the mapping at `base` (header included) viewed in place as `Bar`s.
///
/// # Safety
/// `base` is page-aligned, `64 + 64·n` bytes are mapped there read-only, and they stay mapped
/// for `'a`.
unsafe fn rows_in_place<'a>(base: *const u8, n: usize) -> &'a [Bar] {
    debug_assert!((base as usize) % 64 == 0);
    // SAFETY: the caller guarantees `base + 64 .. base + 64 + 64·n` is mapped, read-only,
    // 64-aligned (page-aligned base + 64) and outlives the borrow tied to `&self` by `rows()`;
    // `Bar` is `repr(C, align(64))` plain integers, so every bit pattern is a valid `Bar`.
    unsafe { core::slice::from_raw_parts(base.add(64).cast::<Bar>(), n) }
}

impl BarFile {
    /// Map the whole file `PROT_READ, MAP_PRIVATE` and validate it: header, length, every row
    /// (I4) and the digest. Zero copy: `rows()` points into the mapping (I9). 7001 on any refusal.
    pub fn open(path: &Path) -> Result<BarFile, BarFileErr> {
        let (f, len, hdr) = open_hdr(path)?;
        check_hdr_shape(&hdr, len)?;
        let n = hdr.n as usize;
        let backing = map_file(&f, len, n)?;
        drop(f); // the mapping keeps the inode
        let file = BarFile {
            hdr,
            backing,
            clamp: None,
        };
        let rows = file.rows();
        let holes = validate_rows(rows, hdr.tf_s)?;
        check_hdr_rows(&hdr, rows, holes)?;
        if src_digest64(rows) != hdr.src_digest64 {
            return Err(BarFileErr::DigestMismatch);
        }
        Ok(file)
    }

    /// Read exactly the rows inside `clamp` into memory this value owns, re-indexed from 0 with
    /// `GAP_BEFORE` cleared on row 0 (I8). Rows outside the clamp are never mapped and never kept:
    /// the bounds come from a binary search that reads 8-byte `open_ms` fields only. Validates the
    /// kept rows; the whole-file digest is `verify_digest`'s job. `Empty` when nothing is inside.
    pub fn open_clamped(path: &Path, clamp: Clamp) -> Result<BarFile, BarFileErr> {
        let (f, len, hdr) = open_hdr(path)?;
        check_hdr_shape(&hdr, len)?;
        let n = hdr.n;
        let open_at = |i: u64| -> Result<i64, BarFileErr> {
            let mut b = [0u8; 8];
            pread_exact(&f, &mut b, REC + REC * i + ROW_OPEN_MS_OFF)?;
            Ok(i64::from_le_bytes(b))
        };
        // First row with open_ms ≥ from.
        let (mut lo, mut hi) = (0u64, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if open_at(mid)? < clamp.from_open_ms {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let first = lo;
        // First row with close_ms > to (close is monotone in open on the grid).
        let (mut lo, mut hi) = (first, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if barclock::close_ms(open_at(mid)?, hdr.tf_s) <= clamp.to_close_ms {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let end = lo;
        if end <= first {
            return Err(BarFileErr::Empty);
        }
        let k = (end - first) as usize;
        let mut rows = vec![Bar::default(); k];
        // COPY: the clamped rows, k × 64 B, once per open — a mapping cannot be shorter than a page,
        // so mapping would put excluded rows in the address space (§4.2.6, I8) — map + slice rejected.
        {
            // SAFETY: `rows` owns `k` initialised `Bar`s (plain integers, every bit pattern valid);
            // viewing them as `64·k` bytes for the read is in bounds and the borrow ends here.
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(rows.as_mut_ptr().cast::<u8>(), k * REC as usize)
            };
            pread_exact(&f, bytes, REC + REC * first)?;
        }
        let mut i = 0;
        while i < k {
            rows[i].index = i as u32;
            i += 1;
        }
        rows[0].flags &= !barflag::GAP_BEFORE;
        validate_rows(&rows, hdr.tf_s)?;
        Ok(BarFile {
            hdr,
            backing: Backing::Owned(rows),
            clamp: Some(clamp),
        })
    }

    /// An owned, validated file built in memory (tests; the hand-off's shadow runs never need it).
    pub fn from_rows(
        rows: Vec<Bar>,
        tf_s: u32,
        symbol_hash: u64,
        venue: u8,
        source: u8,
    ) -> Result<BarFile, BarFileErr> {
        let holes = validate_rows(&rows, tf_s)?;
        let hdr = BarFileHdr::of_rows(&rows, holes, tf_s, symbol_hash, venue, source);
        Ok(BarFile {
            hdr,
            backing: Backing::Owned(rows),
            clamp: None,
        })
    }

    /// Stream the whole file through SHA-256 without mapping it and compare `src_digest64`.
    pub fn verify_digest(path: &Path) -> Result<BarFileHdr, BarFileErr> {
        let (f, len, hdr) = open_hdr(path)?;
        check_hdr_shape(&hdr, len)?;
        let mut h = Sha256::new();
        let mut buf = [0u8; 16_384];
        let mut off = REC;
        while off < len {
            let take = (len - off).min(buf.len() as u64) as usize;
            pread_exact(&f, &mut buf[..take], off)?;
            h.update(&buf[..take]);
            off += take as u64;
        }
        let d = h.finalize();
        if u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]) != hdr.src_digest64
        {
            return Err(BarFileErr::DigestMismatch);
        }
        Ok(hdr)
    }

    /// The header as read (a clamped file keeps the file's header; its rows are `rows()`).
    pub fn hdr(&self) -> &BarFileHdr {
        &self.hdr
    }

    /// The clamp this value was opened with.
    pub fn clamp(&self) -> Option<Clamp> {
        self.clamp
    }

    /// The rows — borrowed, never copied.
    #[inline]
    pub fn rows(&self) -> &[Bar] {
        match &self.backing {
            #[cfg(unix)]
            Backing::Map { ptr, n, .. } => {
                // SAFETY: `map_file` checked `len == 64 + 64·n` and the page-aligned base; the
                // mapping lives until `Drop`, which needs `&mut self`, so it outlives this borrow.
                unsafe { rows_in_place(*ptr, *n) }
            }
            Backing::Owned(v) => v,
        }
    }

    /// First row with `open_ms ≥ t_ms` (binary search; load time).
    pub fn seek(&self, t_ms: i64) -> usize {
        self.rows().partition_point(|b| b.open_ms < t_ms)
    }
}

/// Map `len` bytes of `f` read-only (unix), or read them into owned rows (elsewhere).
#[cfg(unix)]
fn map_file(f: &std::fs::File, len: u64, n: usize) -> Result<Backing, BarFileErr> {
    use std::os::unix::io::AsRawFd;
    let ulen = usize::try_from(len).map_err(|_| BarFileErr::Truncated { len })?;
    // SAFETY: a fresh PROT_READ/MAP_PRIVATE mapping of an open descriptor; `ulen ≥ 64 > 0`; the
    // result is checked against MAP_FAILED before use; nothing else aliases it mutably.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            ulen,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            f.as_raw_fd(),
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io_err(&std::io::Error::last_os_error()));
    }
    Ok(Backing::Map {
        ptr: p as *const u8,
        len: ulen,
        n,
    })
}

#[cfg(not(unix))]
fn map_file(f: &std::fs::File, len: u64, n: usize) -> Result<Backing, BarFileErr> {
    let mut rows = vec![Bar::default(); n];
    // COPY: the whole file's rows, n × 64 B, once per open — no mmap off unix (CAR-07-1 is unix) —
    // a portable mapping crate rejected (no new dependency).
    {
        // SAFETY: `rows` owns `n` initialised plain-integer `Bar`s; the byte view ends here.
        let bytes =
            unsafe { core::slice::from_raw_parts_mut(rows.as_mut_ptr().cast::<u8>(), n * 64) };
        pread_exact(f, bytes, REC)?;
    }
    let _ = len;
    Ok(Backing::Owned(rows))
}

impl Drop for BarFile {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Backing::Map { ptr, len, .. } = self.backing {
            // SAFETY: `ptr`/`len` are exactly what `mmap` returned for this value, unmapped once
            // here; no borrow of `rows()` can outlive `&mut self`.
            unsafe {
                libc::munmap(ptr as *mut libc::c_void, len);
            }
        }
    }
}

/// Validate rows against I4; returns the hole count. What `open`, `from_rows` and the writer run.
pub fn validate_rows(rows: &[Bar], tf_s: u32) -> Result<u32, BarFileErr> {
    if !barclock::is_allowed(tf_s) {
        return Err(BarFileErr::BadTimeframe(tf_s));
    }
    if rows.is_empty() {
        return Err(BarFileErr::Empty);
    }
    const ALLOWED: u32 = barflag::CONFIRMED | barflag::GAP_BEFORE | barflag::VOL_SAT;
    let mut holes: u64 = 0;
    let mut i = 0;
    while i < rows.len() {
        let b = &rows[i];
        let row = i as u64;
        if i > 0 && b.open_ms <= rows[i - 1].open_ms {
            return Err(BarFileErr::NotAscending { row });
        }
        if barclock::open_ms(b.open_ms, tf_s) != b.open_ms {
            return Err(BarFileErr::BoundaryMismatch { row });
        }
        if barclock::close_ms(b.open_ms, tf_s) != b.close_ms {
            return Err(BarFileErr::CloseMismatch { row });
        }
        if u64::from(b.index) != row {
            return Err(BarFileErr::IndexMismatch { row });
        }
        if b.flags & !ALLOWED != 0 || b.flags & barflag::CONFIRMED == 0 {
            return Err(BarFileErr::BadFlags { row });
        }
        // pin:07-gap-row0: row 0 has no row before it, so no hole: a GAP_BEFORE there disagrees
        // with "the hole before the row" (literal reading; I8 clears it on a clamped row 0).
        let gap = if i == 0 {
            0
        } else {
            barclock::bars_between(rows[i - 1].open_ms, b.open_ms, tf_s) - 1
        };
        if (gap > 0) != (b.flags & barflag::GAP_BEFORE != 0) {
            return Err(BarFileErr::GapFlag { row });
        }
        holes += gap as u64;
        let lo = b.open_1e6.min(b.close_1e6);
        let hi = b.open_1e6.max(b.close_1e6);
        if b.low_1e6 <= 0 || b.low_1e6 > lo || hi > b.high_1e6 {
            return Err(BarFileErr::BadPrice { row });
        }
        if b.volume_1e6 < 0 || (b.volume_1e6 == i64::MAX) != (b.flags & barflag::VOL_SAT != 0) {
            return Err(BarFileErr::BadVolume { row });
        }
        i += 1;
    }
    Ok(u32::try_from(holes).unwrap_or(u32::MAX))
}
