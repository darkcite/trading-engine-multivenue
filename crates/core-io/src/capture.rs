//! # PmlrCapture — per-ingress replay capture + bounded raw tap
//!
//! Production implementation of [`core_types::Capture`] (Phase 8e,
//! plan §6.5). Each ingress thread owns one `PmlrCapture`, constructed
//! at boot by the cli spawn wrapper and moved into the thread; it
//! survives reconnects (files stay open for the process lifetime).
//!
//! ## Files (all under the per-run capture directory)
//!
//! * `<venue>-ticks.pmlr`   — [`SlotKind::Tick`] slots (BBO ticks,
//!   captured **before** the ring `try_push`, so ring-dropped ticks are
//!   still visible to the offline audit).
//! * `<venue>-events.pmlr`  — [`SlotKind::Event`] slots (all non-tick
//!   channels: trades, books, mark, funding, ticker, ctx, …).
//! * `<venue>-signals.pmlr` — [`SlotKind::Signal`] slots (RPC ingress;
//!   empty 64 B header-only file on venues that emit none — uniform
//!   construction beats per-venue file sets).
//! * `<venue>-raw.tap`      — optional bounded raw-payload tap, see
//!   below. Only created when the tap mode is not [`TapMode::Off`].
//!
//! ## Raw tap (`--raw-tap`)
//!
//! Byte-exact inbound payload capture for parser-vs-wire differential
//! audits (the Deribit ~1.3 % reject investigation is its first user).
//! Format: a 64 B header (`b"PMRT"`, version, venue byte, epoch_ns)
//! followed by variable-length records
//! `[ts_ns u64][len u32][flags u32][payload len B]` — flags bit 0 set
//! ⇒ the parser rejected this payload. The tap is **budget-bounded**
//! ([`TapCfg::budget_bytes`]): once the budget is exhausted, further
//! records are dropped and counted ([`PmlrCapture::tap_dropped`]).
//! Plan §6.5 sketched a "bounded ring → file"; this is implemented as
//! budget-bounded *first-N* capture instead — deterministic, simpler,
//! and exactly what a reject hunt needs (documented deviation).
//!
//! ## Error policy (deliberate deviation from hot-path fail-fast)
//!
//! Capture is observability: a full disk mid-soak must degrade
//! *capture*, never the market-data session. On any I/O error the
//! capture **sticky-disables** itself (`debug_assert!` in debug
//! builds), counts the error, and every subsequent hook becomes a
//! no-op branch. The cli mirrors [`PmlrCapture::io_errors`] into a
//! gauge so a disabled capture is loudly visible — the G1 soak
//! verdict reads it.
//!
//! ## Zero-alloc contract
//!
//! All allocation happens in [`PmlrCapture::open`] (boot). The steady
//! state hooks copy fixed 64 B slots / bounded payload slices into
//! preallocated staging buffers ([`PreallocatedWriter`]); flushes are
//! plain `write_all` syscalls. Proven by
//! `capture_steady_state_is_zero_alloc` in `bench/tests/alloc_assertions.rs`.

use std::io;
use std::path::Path;

use core_types::{Capture, ChannelEvent, NsTs, Signal, Tick};

use crate::pmlr::{PmlrWriter, SlotKind};
use crate::PreallocatedWriter;

// ---------------------------------------------------------------
// Raw tap format
// ---------------------------------------------------------------

/// Raw-tap file magic at offset 0.
pub const RAW_TAP_MAGIC: [u8; 4] = *b"PMRT";

/// Raw-tap format version.
pub const RAW_TAP_VERSION: u16 = 1;

/// Raw-tap header size (mirrors the PMLR header size).
pub const RAW_TAP_HEADER_SIZE: usize = 64;

/// Fixed per-record overhead: `ts_ns u64 + len u32 + flags u32`.
pub const RAW_TAP_RECORD_OVERHEAD: usize = 16;

/// Record flag bit 0: the parser rejected this payload.
pub const RAW_TAP_FLAG_REJECT: u32 = 1;

/// Default tap budget — bounds the file so a chatty venue cannot fill
/// the disk (64 MiB ≈ 45 min of full Deribit raw at the live-observed
/// ~40 msg/s × ~600 B).
pub const DEFAULT_TAP_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Largest payload the tap will record; longer payloads are truncated
/// to this length (len field records the stored length). Matches the
/// largest ingress rx buffer (Deribit 4 MiB) — in practice frames are
/// far smaller.
pub const RAW_TAP_MAX_PAYLOAD: usize = 4 * 1024 * 1024;

/// What the raw tap records. Off in production runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TapMode {
    /// No tap file at all.
    Off,
    /// Only payloads the parser rejected (`parse_reject` hook).
    Rejects,
    /// Every inbound payload (`raw_frame`) plus rejects.
    All,
}

/// Raw-tap configuration for one [`PmlrCapture`].
#[derive(Copy, Clone, Debug)]
pub struct TapCfg {
    /// Recording mode.
    pub mode: TapMode,
    /// Byte budget for the tap file (records stop once exhausted).
    pub budget_bytes: u64,
}

impl TapCfg {
    /// Tap disabled.
    #[inline]
    pub const fn off() -> Self {
        Self {
            mode: TapMode::Off,
            budget_bytes: 0,
        }
    }
}

// ---------------------------------------------------------------
// Flush cadence
// ---------------------------------------------------------------

/// Staged bytes reach disk at least this often (quiet feeds would
/// otherwise hold data in the 64 KiB staging buffer for hours; a crash
/// would lose it and a live `audit-replay` would read short).
pub const CAPTURE_FLUSH_INTERVAL_NS: u64 = 1_000_000_000;

// ---------------------------------------------------------------
// PmlrCapture
// ---------------------------------------------------------------

/// Per-ingress capture sink. See module docs. Construct via
/// [`PmlrCapture::open`]; use through the [`core_types::Capture`]
/// trait as the run loop's `C: Capture` parameter.
pub struct PmlrCapture {
    ticks: PmlrWriter,
    events: PmlrWriter,
    signals: PmlrWriter,
    tap: Option<PreallocatedWriter>,
    tap_mode: TapMode,
    tap_budget_left: u64,
    tap_records: u64,
    tap_dropped: u64,
    enabled: bool,
    io_errors: u64,
    last_flush_ns: u64,
}

impl PmlrCapture {
    /// Create the per-venue capture files inside `dir` (created if
    /// absent). `venue_label` names the files (`okx-ticks.pmlr`, …);
    /// `epoch_ns` is wall-clock ns at open (PMLR header contract).
    /// Boot-time only — allocates.
    pub fn open<P: AsRef<Path>>(
        dir: P,
        venue_label: &str,
        epoch_ns: u64,
        tap_cfg: TapCfg,
    ) -> io::Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let ticks = PmlrWriter::open(
            dir.join(format!("{venue_label}-ticks.pmlr")),
            SlotKind::Tick,
            epoch_ns,
        )?;
        let events = PmlrWriter::open(
            dir.join(format!("{venue_label}-events.pmlr")),
            SlotKind::Event,
            epoch_ns,
        )?;
        let signals = PmlrWriter::open(
            dir.join(format!("{venue_label}-signals.pmlr")),
            SlotKind::Signal,
            epoch_ns,
        )?;
        let tap = match tap_cfg.mode {
            TapMode::Off => None,
            TapMode::Rejects | TapMode::All => {
                let path = dir.join(format!("{venue_label}-raw.tap"));
                // Header first (truncating create), then hand the same
                // path to the staging writer — the PmlrWriter::open
                // pattern.
                {
                    use std::io::Write;
                    let mut header = [0u8; RAW_TAP_HEADER_SIZE];
                    header[0..4].copy_from_slice(&RAW_TAP_MAGIC);
                    header[4..6].copy_from_slice(&RAW_TAP_VERSION.to_le_bytes());
                    // header[6] is the venue byte — the cli names files
                    // per venue already; the byte is written as 0xFF
                    // (unknown) here and exists for standalone-file
                    // identification by future tooling. Callers that
                    // care can rewrite it via `set_tap_venue_byte`.
                    header[6] = 0xFF;
                    header[8..16].copy_from_slice(&epoch_ns.to_le_bytes());
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&path)?;
                    f.write_all(&header)?;
                    f.sync_data()?;
                }
                Some(PreallocatedWriter::open(&path, 64 * 1024)?)
            }
        };
        Ok(Self {
            ticks,
            events,
            signals,
            tap,
            tap_mode: tap_cfg.mode,
            tap_budget_left: tap_cfg.budget_bytes,
            tap_records: 0,
            tap_dropped: 0,
            enabled: true,
            io_errors: 0,
            last_flush_ns: 0,
        })
    }

    /// Overwrite the venue byte at header offset 6 of the tap file.
    /// Boot-time only (seeks under the staging writer are not
    /// possible, so this must be called before any tap record is
    /// staged — enforced by debug_assert).
    pub fn set_tap_venue_byte(&mut self, dir: &Path, venue_label: &str, venue: u8) -> io::Result<()> {
        debug_assert_eq!(self.tap_records, 0, "venue byte must be set before records");
        if self.tap_mode == TapMode::Off {
            return Ok(());
        }
        use std::io::{Seek, SeekFrom, Write};
        let path = dir.join(format!("{venue_label}-raw.tap"));
        let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
        f.seek(SeekFrom::Start(6))?;
        f.write_all(&[venue])?;
        f.sync_data()?;
        Ok(())
    }

    /// Total I/O errors observed (first one sticky-disables capture).
    #[inline]
    pub fn io_errors(&self) -> u64 {
        self.io_errors
    }

    /// True once an I/O error has sticky-disabled this capture.
    #[inline]
    pub fn is_disabled(&self) -> bool {
        !self.enabled
    }

    /// Tick slots staged since open.
    #[inline]
    pub fn ticks_written(&self) -> u64 {
        self.ticks.records_written()
    }

    /// Event slots staged since open.
    #[inline]
    pub fn events_written(&self) -> u64 {
        self.events.records_written()
    }

    /// Signal slots staged since open.
    #[inline]
    pub fn signals_written(&self) -> u64 {
        self.signals.records_written()
    }

    /// Tap records staged since open.
    #[inline]
    pub fn tap_records(&self) -> u64 {
        self.tap_records
    }

    /// Tap records dropped after the budget ran out.
    #[inline]
    pub fn tap_dropped(&self) -> u64 {
        self.tap_dropped
    }

    /// Drain every staging buffer to disk. Called by `maybe_flush` on
    /// the flush interval and by `Drop`; callable directly at orderly
    /// shutdown.
    pub fn flush_all(&mut self) -> io::Result<()> {
        self.ticks.flush()?;
        self.events.flush()?;
        self.signals.flush()?;
        if let Some(t) = self.tap.as_mut() {
            t.flush_staging()?;
        }
        Ok(())
    }

    /// Sticky-disable on error (see module docs).
    #[cold]
    fn note_io_error(&mut self) {
        self.io_errors += 1;
        self.enabled = false;
        debug_assert!(false, "capture I/O error — sticky-disabled");
    }

    /// Stage one tap record (shared by `raw_frame` / `parse_reject`).
    fn tap_record(&mut self, ts_ns: NsTs, payload: &[u8], flags: u32) {
        let stored = if payload.len() > RAW_TAP_MAX_PAYLOAD {
            RAW_TAP_MAX_PAYLOAD
        } else {
            payload.len()
        };
        let need = (RAW_TAP_RECORD_OVERHEAD + stored) as u64;
        if self.tap_budget_left < need {
            self.tap_dropped += 1;
            return;
        }
        let Some(tap) = self.tap.as_mut() else {
            // Unreachable by construction: tap_mode gates the callers.
            debug_assert!(false, "tap_record without tap writer");
            return;
        };
        let mut rec_header = [0u8; RAW_TAP_RECORD_OVERHEAD];
        rec_header[0..8].copy_from_slice(&ts_ns.to_le_bytes());
        rec_header[8..12].copy_from_slice(&(stored as u32).to_le_bytes());
        rec_header[12..16].copy_from_slice(&flags.to_le_bytes());
        let r = tap
            .push_slice(&rec_header)
            .and_then(|()| tap.push_slice(&payload[..stored]));
        match r {
            Ok(()) => {
                self.tap_budget_left -= need;
                self.tap_records += 1;
            }
            Err(_) => self.note_io_error(),
        }
    }
}

impl Capture for PmlrCapture {
    #[inline]
    fn tick(&mut self, t: &Tick) {
        if !self.enabled {
            return;
        }
        if self.ticks.append(t).is_err() {
            self.note_io_error();
        }
    }

    #[inline]
    fn event(&mut self, e: &ChannelEvent) {
        if !self.enabled {
            return;
        }
        if self.events.append(e).is_err() {
            self.note_io_error();
        }
    }

    #[inline]
    fn signal(&mut self, s: &Signal) {
        if !self.enabled {
            return;
        }
        if self.signals.append(s).is_err() {
            self.note_io_error();
        }
    }

    #[inline]
    fn raw_frame(&mut self, ts_ns: NsTs, payload: &[u8]) {
        if !self.enabled || self.tap_mode != TapMode::All {
            return;
        }
        self.tap_record(ts_ns, payload, 0);
    }

    #[inline]
    fn parse_reject(&mut self, ts_ns: NsTs, payload: &[u8]) {
        if !self.enabled || self.tap_mode == TapMode::Off {
            return;
        }
        self.tap_record(ts_ns, payload, RAW_TAP_FLAG_REJECT);
    }

    #[inline]
    fn maybe_flush(&mut self, now_ns: NsTs) {
        if !self.enabled {
            return;
        }
        if now_ns.wrapping_sub(self.last_flush_ns) < CAPTURE_FLUSH_INTERVAL_NS {
            return;
        }
        self.last_flush_ns = now_ns;
        if self.flush_all().is_err() {
            self.note_io_error();
        }
    }
}

impl Drop for PmlrCapture {
    fn drop(&mut self) {
        // Best-effort final drain; errors are unobservable here and the
        // process is tearing down.
        let _ = self.flush_all();
    }
}

// ---------------------------------------------------------------
// Raw tap reader (offline audit tooling)
// ---------------------------------------------------------------

/// Why a raw-tap file failed to open/parse.
#[derive(Debug)]
pub enum RawTapErr {
    /// Underlying I/O failure.
    Io(io::Error),
    /// Bad magic / version / short header.
    Header,
}

impl ::core::fmt::Display for RawTapErr {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "raw tap i/o: {e}"),
            Self::Header => write!(f, "raw tap header invalid"),
        }
    }
}

impl std::error::Error for RawTapErr {}

/// One decoded tap record borrowed from the reader's buffer.
#[derive(Copy, Clone, Debug)]
pub struct RawTapRecord<'a> {
    /// Capture timestamp (ingress monotonic clock).
    pub ts_ns: u64,
    /// Record flags ([`RAW_TAP_FLAG_REJECT`]).
    pub flags: u32,
    /// The stored payload bytes.
    pub payload: &'a [u8],
}

impl RawTapRecord<'_> {
    /// True if the parser rejected this payload.
    #[inline]
    pub fn is_reject(&self) -> bool {
        self.flags & RAW_TAP_FLAG_REJECT != 0
    }
}

/// Offline reader for `<venue>-raw.tap` files. Loads the whole file
/// (audit tooling — allocation fine); iterate with
/// [`RawTapReader::next_record`]. A record extending past EOF (torn
/// final write) terminates iteration silently — the readable prefix
/// is exactly what the audit consumes.
pub struct RawTapReader {
    buf: Vec<u8>,
    pos: usize,
    venue: u8,
    epoch_ns: u64,
}

impl RawTapReader {
    /// Open and validate a tap file.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, RawTapErr> {
        let buf = std::fs::read(path).map_err(RawTapErr::Io)?;
        if buf.len() < RAW_TAP_HEADER_SIZE
            || buf[0..4] != RAW_TAP_MAGIC
            || u16::from_le_bytes([buf[4], buf[5]]) != RAW_TAP_VERSION
        {
            return Err(RawTapErr::Header);
        }
        let venue = buf[6];
        let mut e = [0u8; 8];
        e.copy_from_slice(&buf[8..16]);
        Ok(Self {
            buf,
            pos: RAW_TAP_HEADER_SIZE,
            venue,
            epoch_ns: u64::from_le_bytes(e),
        })
    }

    /// Venue byte from the header (0xFF = unset).
    #[inline]
    pub fn venue(&self) -> u8 {
        self.venue
    }

    /// Wall-clock ns at file open.
    #[inline]
    pub fn epoch_ns(&self) -> u64 {
        self.epoch_ns
    }

    /// Next record, or `None` at EOF / torn tail.
    pub fn next_record(&mut self) -> Option<RawTapRecord<'_>> {
        let remain = self.buf.len() - self.pos;
        if remain < RAW_TAP_RECORD_OVERHEAD {
            return None;
        }
        let p = self.pos;
        let mut t = [0u8; 8];
        t.copy_from_slice(&self.buf[p..p + 8]);
        let ts_ns = u64::from_le_bytes(t);
        let len = u32::from_le_bytes([
            self.buf[p + 8],
            self.buf[p + 9],
            self.buf[p + 10],
            self.buf[p + 11],
        ]) as usize;
        let flags = u32::from_le_bytes([
            self.buf[p + 12],
            self.buf[p + 13],
            self.buf[p + 14],
            self.buf[p + 15],
        ]);
        let start = p + RAW_TAP_RECORD_OVERHEAD;
        let end = start.checked_add(len)?;
        if end > self.buf.len() {
            // Torn final record.
            return None;
        }
        self.pos = end;
        Some(RawTapRecord {
            ts_ns,
            flags,
            payload: &self.buf[start..end],
        })
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{ChannelId, LatencyClass, Price, Qty, SignalSource, VenueId};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pmlr_capture_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn one_tick() -> Tick {
        Tick::new(
            10,
            VenueId::Okx,
            core_types::make_symbol_id(VenueId::Okx, 1),
            7,
            Price::from_raw(100_000_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(100_100_000),
            Qty::from_raw(2_000_000),
        )
    }

    fn one_event() -> ChannelEvent {
        ChannelEvent::new(
            11,
            VenueId::Okx,
            ChannelId::Trade,
            core_types::make_symbol_id(VenueId::Okx, 1),
            42,
            1_700_000_000_000,
            100_050_000,
            5_000_000,
        )
    }

    #[test]
    fn capture_roundtrips_all_three_pmlr_files() {
        let dir = temp_dir("roundtrip");
        let mut c = PmlrCapture::open(&dir, "okx", 123, TapCfg::off()).unwrap();
        let t = one_tick();
        let e = one_event();
        let s = Signal::new(12, 0, LatencyClass::Slow, SignalSource::Rpc as u8, [7; 40]);
        c.tick(&t);
        c.event(&e);
        c.event(&e);
        c.signal(&s);
        c.flush_all().unwrap();
        assert_eq!(c.ticks_written(), 1);
        assert_eq!(c.events_written(), 2);
        assert_eq!(c.signals_written(), 1);
        assert_eq!(c.io_errors(), 0);
        assert!(!c.is_disabled());

        let r = crate::PmlrReader::<Tick>::open(dir.join("okx-ticks.pmlr")).unwrap();
        assert_eq!(r.slot_kind(), SlotKind::Tick);
        assert_eq!(r.len(), 1);
        assert_eq!(r.records()[0].venue_seq, 7);

        let r = crate::PmlrReader::<ChannelEvent>::open(dir.join("okx-events.pmlr")).unwrap();
        assert_eq!(r.slot_kind(), SlotKind::Event);
        assert_eq!(r.epoch_ns(), 123);
        assert_eq!(r.len(), 2);
        assert_eq!(r.records()[0].channel, ChannelId::Trade as u8);
        assert_eq!(r.records()[1].venue_seq, 42);

        let r = crate::PmlrReader::<Signal>::open(dir.join("okx-signals.pmlr")).unwrap();
        assert_eq!(r.slot_kind(), SlotKind::Signal);
        assert_eq!(r.len(), 1);

        // No tap file in Off mode.
        assert!(!dir.join("okx-raw.tap").exists());
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tap_all_mode_records_frames_and_rejects_with_flags() {
        let dir = temp_dir("tap_all");
        let mut c = PmlrCapture::open(
            &dir,
            "deribit",
            9,
            TapCfg {
                mode: TapMode::All,
                budget_bytes: 4096,
            },
        )
        .unwrap();
        c.raw_frame(100, b"good frame");
        c.parse_reject(200, b"bad frame");
        c.maybe_flush(CAPTURE_FLUSH_INTERVAL_NS + 1);
        assert_eq!(c.tap_records(), 2);
        assert_eq!(c.tap_dropped(), 0);

        let mut r = RawTapReader::open(dir.join("deribit-raw.tap")).unwrap();
        assert_eq!(r.epoch_ns(), 9);
        let rec = r.next_record().unwrap();
        assert_eq!(rec.ts_ns, 100);
        assert!(!rec.is_reject());
        assert_eq!(rec.payload, b"good frame");
        let rec = r.next_record().unwrap();
        assert_eq!(rec.ts_ns, 200);
        assert!(rec.is_reject());
        assert_eq!(rec.payload, b"bad frame");
        assert!(r.next_record().is_none());
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tap_rejects_mode_ignores_raw_frames() {
        let dir = temp_dir("tap_rej");
        let mut c = PmlrCapture::open(
            &dir,
            "deribit",
            0,
            TapCfg {
                mode: TapMode::Rejects,
                budget_bytes: 4096,
            },
        )
        .unwrap();
        c.raw_frame(1, b"ignored");
        c.parse_reject(2, b"kept");
        c.flush_all().unwrap();
        assert_eq!(c.tap_records(), 1);
        let mut r = RawTapReader::open(dir.join("deribit-raw.tap")).unwrap();
        let rec = r.next_record().unwrap();
        assert_eq!(rec.payload, b"kept");
        assert!(rec.is_reject());
        assert!(r.next_record().is_none());
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tap_budget_exhaustion_drops_and_counts() {
        let dir = temp_dir("tap_budget");
        // Budget fits exactly one 16+8 record.
        let mut c = PmlrCapture::open(
            &dir,
            "okx",
            0,
            TapCfg {
                mode: TapMode::All,
                budget_bytes: (RAW_TAP_RECORD_OVERHEAD + 8) as u64,
            },
        )
        .unwrap();
        c.raw_frame(1, b"12345678");
        c.raw_frame(2, b"12345678");
        c.raw_frame(3, b"x");
        c.flush_all().unwrap();
        assert_eq!(c.tap_records(), 1);
        assert_eq!(c.tap_dropped(), 2);
        let mut r = RawTapReader::open(dir.join("okx-raw.tap")).unwrap();
        assert_eq!(r.next_record().unwrap().ts_ns, 1);
        assert!(r.next_record().is_none());
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maybe_flush_respects_interval() {
        let dir = temp_dir("flush_interval");
        let mut c = PmlrCapture::open(&dir, "bn", 0, TapCfg::off()).unwrap();
        c.tick(&one_tick());
        // Below the interval: staging not yet drained (file holds only
        // the 64 B header).
        c.maybe_flush(CAPTURE_FLUSH_INTERVAL_NS - 1);
        assert_eq!(
            std::fs::metadata(dir.join("bn-ticks.pmlr")).unwrap().len(),
            64
        );
        // Interval reached: drained.
        c.maybe_flush(CAPTURE_FLUSH_INTERVAL_NS);
        assert_eq!(
            std::fs::metadata(dir.join("bn-ticks.pmlr")).unwrap().len(),
            128
        );
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_drains_staging() {
        let dir = temp_dir("drop_drain");
        {
            let mut c = PmlrCapture::open(&dir, "pm", 0, TapCfg::off()).unwrap();
            c.tick(&one_tick());
            // No explicit flush.
        }
        let r = crate::PmlrReader::<Tick>::open(dir.join("pm-ticks.pmlr")).unwrap();
        assert_eq!(r.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_fails_on_unwritable_dir() {
        // A regular FILE where the directory should be → create_dir_all
        // must fail.
        let base = temp_dir("unwritable");
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let r = PmlrCapture::open(&blocker, "okx", 0, TapCfg::off());
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn set_tap_venue_byte_rewrites_header() {
        let dir = temp_dir("venue_byte");
        let mut c = PmlrCapture::open(
            &dir,
            "hl",
            0,
            TapCfg {
                mode: TapMode::All,
                budget_bytes: 1024,
            },
        )
        .unwrap();
        c.set_tap_venue_byte(&dir, "hl", VenueId::Hyperliquid.to_u8())
            .unwrap();
        c.raw_frame(1, b"z");
        c.flush_all().unwrap();
        let r = RawTapReader::open(dir.join("hl-raw.tap")).unwrap();
        assert_eq!(r.venue(), VenueId::Hyperliquid.to_u8());
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_tap_reader_rejects_bad_header_and_handles_torn_tail() {
        let dir = temp_dir("tap_reader_err");
        std::fs::create_dir_all(&dir).unwrap();
        // Bad magic.
        let bad = dir.join("bad.tap");
        std::fs::write(&bad, [0u8; RAW_TAP_HEADER_SIZE]).unwrap();
        assert!(matches!(
            RawTapReader::open(&bad),
            Err(RawTapErr::Header)
        ));
        // Short file.
        let short = dir.join("short.tap");
        std::fs::write(&short, b"PMRT").unwrap();
        assert!(matches!(
            RawTapReader::open(&short),
            Err(RawTapErr::Header)
        ));
        // Torn tail: header + record header claiming 100 B payload but
        // only 3 present.
        let torn = dir.join("torn.tap");
        let mut buf = Vec::new();
        buf.extend_from_slice(&RAW_TAP_MAGIC);
        buf.extend_from_slice(&RAW_TAP_VERSION.to_le_bytes());
        buf.resize(RAW_TAP_HEADER_SIZE, 0);
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(b"abc");
        std::fs::write(&torn, &buf).unwrap();
        let mut r = RawTapReader::open(&torn).unwrap();
        assert!(r.next_record().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
