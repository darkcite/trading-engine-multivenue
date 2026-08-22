//! # PMLR — Polymarket Replay Log writer
//!
//! Binary append-only log of 64-byte fixed-width slots, designed to be
//! memory-mapped by the replay / backtest reader. One file, one slot
//! kind: a tick log, a signal log, a fill log, or an order log.
//!
//! ## On-disk shape
//!
//! ```text
//!     ┌──── 64 B header ────┐┌── 64 B slot ──┐┌── 64 B slot ──┐  ...
//!     │ b"PMLR" | v | kind │ │  POD record  │ │  POD record  │
//!     └─────────────────────┘└──────────────┘└──────────────┘
//! ```
//!
//! The header is documented in `docs/wire-format.md`. Readers mmap the
//! file and slice it at byte 64; no per-slot framing bytes to skip.
//!
//! ## Zero-alloc contract
//!
//! * The staging buffer is allocated exactly once in
//!   [`PmlrWriter::open`].
//! * [`PmlrWriter::append`] copies one 64 B record into the staging
//!   buffer. When the staging buffer hits capacity, a single
//!   `write_all` flushes it; no dynamic sizing.
//! * Payload bytes reach disk via a direct `&[u8]` view over the
//!   record. No serde, no `to_vec`, no intermediate allocations.

use std::io;
use std::path::Path;

use core_types::AsBytes;

use crate::PreallocatedWriter;

// ---------------------------------------------------------------
// Header layout — see docs/wire-format.md
// ---------------------------------------------------------------

/// Size of a single PMLR slot. Matches the 64 B cache-line of every
/// `core_types` POD struct used here.
pub const SLOT_SIZE: usize = 64;

/// Size of the PMLR file header.
pub const HEADER_SIZE: usize = 64;

/// Magic bytes at file offset 0.
pub const MAGIC: [u8; 4] = *b"PMLR";

/// Wire format version. Bumped on any slot-layout change.
///
/// * v1 — Phase 1 layouts: no venue byte, implicit tail padding
///   (contents of those 8 bytes are undefined in v1 files).
/// * v2 — Phase 8a: `Tick.venue` at offset 48, `Order.venue` at
///   offset 40, all padding explicit and zeroed. v1 files remain
///   readable but are venue-less; see `docs/migration.md`.
pub const VERSION: u16 = 2;

/// Default staging buffer size. 64 KiB == 1024 slots — amortises the
/// `write_all` syscall rate over ~65k records at steady state.
pub const DEFAULT_STAGING_SIZE: usize = 64 * 1024;

/// The kind of record a PMLR file carries. Encoded as one byte in the
/// header and statically paired with a Rust type via the typed
/// [`PmlrWriter::append`] call.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SlotKind {
    /// [`core_types::Tick`].
    Tick = 0,
    /// [`core_types::Signal`].
    Signal = 1,
    /// [`core_types::Fill`].
    Fill = 2,
    /// [`core_types::Order`].
    Order = 3,
    /// [`core_types::AiCmd`] — AI-ingress command capture
    /// (Phase 8f, plan §8.4).
    AiCmd = 4,
    /// [`core_types::ChannelEvent`] — non-tick channel capture
    /// (Phase 8e, plan §6.5).
    Event = 5,
    /// [`core_types::OptSummary`] — options analytics capture
    /// (M2.3, mvp-plan §4-M2.3/§9.8; docs/m2-progress.md).
    OptSummary = 6,
}

impl SlotKind {
    /// Raw byte value written into the header.
    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Decode a byte from a mmap'd header. Returns `None` for unknown
    /// values — a reader should treat that as file corruption.
    #[inline]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Tick),
            1 => Some(Self::Signal),
            2 => Some(Self::Fill),
            3 => Some(Self::Order),
            4 => Some(Self::AiCmd),
            5 => Some(Self::Event),
            6 => Some(Self::OptSummary),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------
// Writer
// ---------------------------------------------------------------

/// Append-only writer for a PMLR replay log.
///
/// Holds a preallocated staging buffer; every [`Self::append`] copies
/// the record into the buffer and flushes to disk when the buffer
/// fills. The caller drives flush cadence explicitly via
/// [`Self::flush`].
pub struct PmlrWriter {
    inner: PreallocatedWriter,
    slot_kind: SlotKind,
    records_written: u64,
}

impl PmlrWriter {
    /// Create or truncate `path` and write the 64-byte PMLR header.
    /// `epoch_ns` is the wall-clock ns at file creation — used by
    /// readers to convert monotonic ts_ns on records to human time.
    pub fn open<P: AsRef<Path>>(
        path: P,
        slot_kind: SlotKind,
        epoch_ns: u64,
    ) -> io::Result<Self> {
        // We cannot use the existing `PreallocatedWriter` directly
        // because it opens in `append` mode — a PMLR file always
        // starts with a fresh header. Open with write+create+truncate
        // ourselves, then hand the file off.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        // Build the header on the stack.
        let mut header = [0u8; HEADER_SIZE];
        header[0..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&VERSION.to_le_bytes());
        header[6] = slot_kind.to_u8();
        // header[7] reserved.
        header[8..16].copy_from_slice(&epoch_ns.to_le_bytes());
        // header[16..64] reserved — already zeroed.

        // Write header synchronously, then wrap the file in our usual
        // staging-buffer writer for steady-state appends. We have to
        // re-open in append mode since the underlying type uses
        // `OpenOptions::append`; simpler to defer open to the helper.
        {
            use std::io::Write;
            let mut f = file;
            f.write_all(&header)?;
            f.sync_data()?;
        }

        // Now open a PreallocatedWriter over the same path for the
        // append stream.
        let inner = PreallocatedWriter::open(&path, DEFAULT_STAGING_SIZE)?;
        Ok(Self {
            inner,
            slot_kind,
            records_written: 0,
        })
    }

    /// Record kind carried by this log.
    #[inline]
    pub fn slot_kind(&self) -> SlotKind {
        self.slot_kind
    }

    /// Number of records successfully staged since open (includes
    /// records still in the staging buffer, awaiting flush).
    #[inline]
    pub fn records_written(&self) -> u64 {
        self.records_written
    }

    /// Append a single record to the log. Zero-alloc.
    ///
    /// # Panics
    ///
    /// Debug builds assert that `size_of::<R>() == SLOT_SIZE`; release
    /// builds silently would-be-corrupt the file if a caller ever
    /// supplies a non-64-byte record. The `AsBytes` marker trait is
    /// only implemented for the four hot-path types, all of which
    /// statically assert 64 B at build time, so this is unreachable in
    /// practice.
    #[inline]
    pub fn append<R: AsBytes>(&mut self, record: &R) -> io::Result<()> {
        debug_assert_eq!(
            core::mem::size_of::<R>(),
            SLOT_SIZE,
            "PMLR records must be exactly 64 bytes",
        );
        // SAFETY: `R: AsBytes` is an unsafe marker trait whose impls
        // promise `R` is `#[repr(C)] + Copy` with no uninitialized
        // padding. The pointer is valid for reads up to
        // `size_of::<R>()` bytes — `record` is a live reference the
        // caller owns. The returned slice does not outlive `record`.
        let bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(
                (record as *const R).cast::<u8>(),
                core::mem::size_of::<R>(),
            )
        };
        self.inner.push_slice(bytes)?;
        self.records_written += 1;
        Ok(())
    }

    /// Drain the staging buffer to disk. Call between batches, or
    /// before measuring p99 latency, or before process exit.
    #[inline]
    pub fn flush(&mut self) -> io::Result<()> {
        self.inner.flush_staging()
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Fill, LatencyClass, Order, Price, Qty, Side, Signal, SignalSource, Tick, VenueId};
    use std::fs::File;
    use std::io::Read;

    fn temp_path(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        dir.join(format!(
            "pmlr_{}_{}_{}.log",
            suffix,
            std::process::id(),
            // A low-entropy cookie to avoid cross-test collision when
            // run in parallel with --test-threads=N (currently 1 only,
            // but future-proof).
            core::sync::atomic::AtomicU64::new(0).fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn slot_kind_roundtrip() {
        assert_eq!(SlotKind::from_u8(0), Some(SlotKind::Tick));
        assert_eq!(SlotKind::from_u8(1), Some(SlotKind::Signal));
        assert_eq!(SlotKind::from_u8(2), Some(SlotKind::Fill));
        assert_eq!(SlotKind::from_u8(3), Some(SlotKind::Order));
        // 4 un-reserved in 8f: the AiCmd capture kind (plan §8.4).
        assert_eq!(SlotKind::from_u8(4), Some(SlotKind::AiCmd));
        assert_eq!(SlotKind::from_u8(5), Some(SlotKind::Event));
        assert_eq!(SlotKind::from_u8(6), Some(SlotKind::OptSummary));
        assert_eq!(SlotKind::from_u8(42), None);
    }

    #[test]
    fn header_is_written_with_correct_magic_and_version() {
        let p = temp_path("header");
        let _ = std::fs::remove_file(&p);
        let mut w = PmlrWriter::open(&p, SlotKind::Tick, 0xDEAD_BEEF_1234_5678).unwrap();
        w.flush().unwrap();

        let mut buf = [0u8; HEADER_SIZE];
        let mut f = File::open(&p).unwrap();
        f.read_exact(&mut buf).unwrap();

        assert_eq!(&buf[0..4], &MAGIC);
        assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), VERSION);
        assert_eq!(buf[6], SlotKind::Tick.to_u8());
        let mut ep = [0u8; 8];
        ep.copy_from_slice(&buf[8..16]);
        assert_eq!(u64::from_le_bytes(ep), 0xDEAD_BEEF_1234_5678);

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn append_tick_roundtrips_bytes() {
        let p = temp_path("tick");
        let _ = std::fs::remove_file(&p);
        let mut w = PmlrWriter::open(&p, SlotKind::Tick, 0).unwrap();

        let t = Tick::new(
            1_000,
            VenueId::Polymarket,
            7,
            42,
            Price::from_raw(518_000),
            Qty::from_raw(100),
            Price::from_raw(520_000),
            Qty::from_raw(50),
        );
        w.append(&t).unwrap();
        w.flush().unwrap();
        assert_eq!(w.records_written(), 1);

        // Read the whole file back.
        let mut bytes = Vec::new();
        File::open(&p).unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), HEADER_SIZE + SLOT_SIZE);

        // Slot 0 should be byte-identical to the source record.
        // SAFETY: Tick is `AsBytes` (`#[repr(C)] + Copy`); producing a
        // byte view of a live reference for a read-only assertion is
        // sound and the slice does not outlive `t`.
        let record_bytes = unsafe {
            core::slice::from_raw_parts((&t as *const Tick).cast::<u8>(), SLOT_SIZE)
        };
        assert_eq!(&bytes[HEADER_SIZE..HEADER_SIZE + SLOT_SIZE], record_bytes);

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn append_many_records_flushes_in_batches() {
        let p = temp_path("many");
        let _ = std::fs::remove_file(&p);
        let mut w = PmlrWriter::open(&p, SlotKind::Signal, 1).unwrap();

        for i in 0..2048 {
            let s = Signal::new(
                i as u64,
                0,
                LatencyClass::Hot,
                SignalSource::Binance as u8,
                [0; 40],
            );
            w.append(&s).unwrap();
        }
        w.flush().unwrap();
        assert_eq!(w.records_written(), 2048);

        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(meta.len(), HEADER_SIZE as u64 + 2048u64 * SLOT_SIZE as u64);

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn append_fill_records_expected_bytes() {
        let p = temp_path("fill");
        let _ = std::fs::remove_file(&p);
        let mut w = PmlrWriter::open(&p, SlotKind::Fill, 0).unwrap();
        let f = Fill::new(
            100,
            9,
            Side::Bid,
            Price::from_raw(500_000),
            Qty::from_raw(2_000),
            0xABCD,
        );
        w.append(&f).unwrap();
        w.flush().unwrap();

        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(meta.len(), HEADER_SIZE as u64 + SLOT_SIZE as u64);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn append_order_records_expected_bytes() {
        let p = temp_path("order");
        let _ = std::fs::remove_file(&p);
        let mut w = PmlrWriter::open(&p, SlotKind::Order, 0).unwrap();
        let o = Order::new(
            200,
            VenueId::Polymarket,
            11,
            Side::Ask,
            0,
            Price::from_raw(600_000),
            Qty::from_raw(1_000),
            0x1234,
        );
        w.append(&o).unwrap();
        w.flush().unwrap();

        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(meta.len(), HEADER_SIZE as u64 + SLOT_SIZE as u64);
        let _ = std::fs::remove_file(&p);
    }
}
