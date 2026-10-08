// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The `binance-exec.pmlr` journal (plan §3.13; BX-14).**
//!
//! What the venue said, in order, for the fee measurement (BX-14) and the
//! shadow reconciliation tool: ACKs, refusals with their code, fills with
//! commission and maker flag, cancels and expiries with their reason,
//! reconciliation verdicts and margin samples — one
//! [`core_types::ExecRecord`] (64 B) each.
//!
//! **Out of band.** The gateway never touches the disk: it pushes records
//! into an SPSC ring ([`JournalTx::note`]) and a cold thread
//! ([`spawn_writer`]) appends them to the file and flushes once a second.
//! A full ring DROPS the record and counts it — the journal is evidence,
//! never a brake on the order path.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core_io::pmlr::SlotKind;
use core_io::slot_capture::SlotCapture;
use core_ring::{Consumer, Producer};
use core_types::ExecRecord;

use crate::mode::AccountMode;

/// The ring between the gateway and the writer.
pub const JOURNAL_RING: usize = 1_024;

/// An ACK: `a` = 0.
pub const J_ACK: u8 = 1;
/// A refusal: `code`.
pub const J_REJECT: u8 = 2;
/// A fill: `a` = price ×1e6, `b` = quantity ×1e6 (signed by side), `c` =
/// commission ×1e6; `code` = 1 if the fill was a maker, plus
/// [`J_FILL_LOST`] if it never reached lane 4.
pub const J_FILL: u8 = 3;
/// [`J_FILL`] `code` bit: the fill was LOST — lane 4 and the held queue
/// were both full — and nothing was booked (the pulse raises unbounded
/// drift).
pub const J_FILL_LOST: i32 = 2;
/// [`J_FILL`] `code` bit: booked without its trade — from an
/// `order.status` answer or a stream update's cumulative quantity — at the
/// implied average price, with no commission or maker flag of its own.
pub const J_FILL_UNSEEN: i32 = 4;
/// [`J_FILL`] `code` bit: the venue's position ADOPTED at boot as the owner
/// slot's starting inventory (BX7; `FILL_FLAG_ADOPTED`) — no trade, no
/// commission. BX-14's trade statistics exclude it.
pub const J_FILL_ADOPTED: i32 = 8;
/// An order ended without a further fill: `code` = the `RETIRED_*` why.
pub const J_ENDED: u8 = 4;
/// A reconciliation verdict: `a` = drift USD ×1e6, `b` = unseen legs,
/// `c` = foreign orders; `code` = 1 if reconciled.
pub const J_RECON: u8 = 5;
/// A margin sample: `a` = ratio ×1e6 (the product's), `b` = the session
/// equity USD ×1e6 (USDⓈ-M's on a classic account, the unified account's
/// in PM and PM Pro), `c` = 1 for a margin call, `code` = the product
/// (`PRODUCT_*`, BX7).
pub const J_MARGIN: u8 = 6;
/// A venue lock or budget observation: `code`.
pub const J_VENUE: u8 = 7;
/// A sweep's result: `a` = orders left, `b` = cancels sent.
pub const J_SWEEP: u8 = 8;
/// The E7 session anchor was set: `a` = equity USD ×1e6. The writer
/// persists it ([`AnchorFile`]) — the gateway thread never writes a file.
pub const J_ANCHOR: u8 = 9;

/// The gateway's end.
pub struct JournalTx {
    p: Producer<ExecRecord, JOURNAL_RING>,
    dropped: u64,
}

impl JournalTx {
    /// Over the ring's producer.
    #[must_use]
    pub const fn new(p: Producer<ExecRecord, JOURNAL_RING>) -> Self {
        Self { p, dropped: 0 }
    }

    /// Journal one record (a full ring drops it and counts). `false`:
    /// dropped — a record that must land (the E7 anchor) is offered again.
    #[inline]
    pub fn note(&mut self, r: &ExecRecord) -> bool {
        if self.p.try_push_ref(r) {
            return true;
        }
        self.dropped += 1;
        false
    }

    /// Offer a record already counted dropped once (the E7 anchor's
    /// retry): `false` if the ring is still full — not counted again.
    #[inline]
    pub fn offer(&mut self, r: &ExecRecord) -> bool {
        self.p.try_push_ref(r)
    }

    /// Records dropped on a full ring.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// **The cold writer**: a thread named `bn-journal` that drains `rx` into
/// `path` until `stop` is set and the ring is empty, then flushes. The
/// file is created (truncated) here, at boot; an open failure refuses. A
/// [`J_ANCHOR`] record is also persisted to `anchor` (the E7 session) —
/// retried every [`ANCHOR_RETRY_NS`] until the store succeeds, with the
/// anchor's `unsaved` flag raised meanwhile (F2: a full disk or a
/// permission error must not lose the session silently).
pub fn spawn_writer(
    path: &Path,
    epoch_ns: u64,
    mut rx: Consumer<ExecRecord, JOURNAL_RING>,
    stop: Arc<AtomicBool>,
    anchor: Option<AnchorFile>,
) -> std::io::Result<std::thread::JoinHandle<u64>> {
    let mut cap: SlotCapture<ExecRecord> = SlotCapture::open(path, SlotKind::Exec, epoch_ns)?;
    std::thread::Builder::new().name(String::from("bn-journal")).spawn(move || {
        // The anchor still to store, and when to try again.
        let mut unstored: Option<i64> = None;
        let mut retry_ns = 0u64;
        loop {
            let mut n = 0;
            while let Some(g) = rx.try_pop_ref() {
                if g.kind == J_ANCHOR {
                    unstored = Some(g.a);
                    retry_ns = 0;
                }
                cap.append(&g);
                n += 1;
            }
            let now = core_time::now_ns();
            if let (Some(a), Some(f)) = (unstored, anchor.as_ref()) {
                if now >= retry_ns {
                    let stored = f.store(a).is_ok();
                    f.unsaved.store(!stored, Ordering::Release);
                    unstored = if stored { None } else { Some(a) };
                    retry_ns = now + ANCHOR_RETRY_NS;
                }
            }
            cap.maybe_flush(now);
            if n == 0 {
                if stop.load(Ordering::Acquire) && rx.is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        // One last try: an anchor that never landed leaves the flag raised
        // for the boot's exit line.
        if let (Some(a), Some(f)) = (unstored, anchor.as_ref()) {
            f.unsaved.store(f.store(a).is_err(), Ordering::Release);
        }
        let _ = cap.flush_all();
        cap.records()
    })
}

/// How often the writer retries an anchor it could not store (F2).
pub const ANCHOR_RETRY_NS: u64 = 1_000_000_000;

/// The longest anchor file read (BX7): `store` writes ≤ 64 B; a longer
/// file is no anchor line, refused unread past this.
const ANCHOR_READ_MAX: u64 = 128;

/// **The E7 session anchor, persisted** (`binance-pnl-anchor.state`).
///
/// The session bound is judged from the account's equity at the first
/// reconciliation of the SESSION, and a session outlives a process (the
/// restart lane relaunches the engine): the anchor is written once and read
/// at every boot; the operator ends a session by deleting the file. One
/// line: `<account tag>\t<equity ×1e6>\t<unix s>\t<account mode>`. The tag
/// is the first 8 bytes of SHA-256 of the API key, in hex — never the key.
///
/// The file is ONE session's (BX7, the risk review's N2): the equity's
/// basis is the account mode's — USDⓈ-M's margin balance on a classic
/// account, the unified account's in PM and PM Pro — so a boot under
/// another key or in another mode is refused until the operator ends that
/// session: never inherited, never silently replaced (which would forget
/// its losses). `signing = "hmac"` signs with another key: toggling it ends
/// the session the same way. A line written before BX7 names no mode — a
/// classic session's. Boot and one cold write only.
#[derive(Clone, Debug)]
pub struct AnchorFile {
    path: std::path::PathBuf,
    tag: [u8; 16],
    /// The account mode the session's equity is read in.
    mode: AccountMode,
    /// The anchor could not be stored (yet): raised by the writer while
    /// its store fails, read by the gateway's pulse (F2).
    unsaved: Arc<AtomicBool>,
}

impl AnchorFile {
    /// The file at `path`, for the account whose API key is `api_key`, in
    /// account mode `mode`.
    #[must_use]
    pub fn new(path: std::path::PathBuf, api_key: &str, mode: AccountMode) -> Self {
        let h = core_crypto::sha256(api_key.as_bytes());
        let hex = b"0123456789abcdef";
        let mut tag = [0u8; 16];
        let mut i = 0;
        while i < 8 {
            tag[2 * i] = hex[(h[i] >> 4) as usize];
            tag[2 * i + 1] = hex[(h[i] & 0x0f) as usize];
            i += 1;
        }
        Self {
            path,
            tag,
            mode,
            unsaved: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The "could not be stored" flag, shared with the gateway (F2).
    #[must_use]
    pub fn unsaved_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.unsaved)
    }

    /// The session's anchor (boot): `None` when no session is open (no
    /// file). `Err` — the boot refuses, naming the remedy and never the key
    /// — when the file is another key's session or another account mode's,
    /// or cannot be read as one.
    ///
    /// # Errors
    /// The file is not this session's, or not an anchor.
    pub fn load(&self) -> Result<Option<i64>, String> {
        // COPY: the anchor file (one line: ≤ 64 B as `store` writes it; a
        // file past `ANCHOR_READ_MAX` is refused unread past it) into a
        // String — boot only, once; the file is closed before anything
        // trades — a stack buffer read was rejected: the boot may allocate,
        // and this is its one read of the file.
        let mut text = String::new();
        let read = std::fs::File::open(&self.path)
            .and_then(|f| std::io::Read::read_to_string(&mut std::io::Read::take(f, ANCHOR_READ_MAX + 1), &mut text));
        match read {
            Ok(n) if n as u64 > ANCHOR_READ_MAX => {
                return Err(String::from("is not an anchor line — delete it to end the session"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot be read ({e}) — fix it, or delete it to end the session")),
        }
        let mut it = text.trim_end().split('\t');
        let (tag, v, unix, mode, extra) = (it.next(), it.next(), it.next(), it.next(), it.next());
        let tag = tag.filter(|t| t.len() == 16 && t.bytes().all(|b| b.is_ascii_hexdigit()));
        let v = v.and_then(|v| v.parse::<i64>().ok()).filter(|v| *v > 0);
        let unix = unix.and_then(|u| u.parse::<u64>().ok());
        let (Some(tag), Some(v), Some(_), None) = (tag, v, unix, extra) else {
            return Err(String::from("is not an anchor line — delete it to end the session"));
        };
        if tag.as_bytes() != self.tag {
            return Err(String::from(
                "holds another API key's session (`signing` picks the key) — delete it to end \
                 that session",
            ));
        }
        // A line written before BX7 names no mode: a classic session's.
        let theirs = mode.map_or(Some(AccountMode::Classic), AccountMode::from_word);
        if theirs != Some(self.mode) {
            return Err(format!(
                "holds a `{}` session and this boot's account is `{}` — the equity's basis \
                 differs; delete it to end that session",
                theirs.map_or("unknown", AccountMode::word),
                self.mode.word()
            ));
        }
        Ok(Some(v))
    }

    /// Persist the anchor (once per session; cold).
    pub fn store(&self, equity_1e6: i64) -> Result<(), String> {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let tag = core::str::from_utf8(&self.tag).unwrap_or("");
        core_io::write_atomic(&self.path, &format!("{tag}\t{equity_1e6}\t{unix}\t{}\n", self.mode.word()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_ring::Ring;

    #[test]
    fn records_reach_the_file_and_a_full_ring_drops() {
        let dir = std::env::temp_dir().join(format!("bn-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("binance-exec.pmlr");
        let (p, c) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
        let mut tx = JournalTx::new(p);
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_writer(&path, 1, c, stop.clone(), None).unwrap();
        for i in 0..10 {
            tx.note(&ExecRecord { kind: J_FILL, client_oid: i, ..ExecRecord::default() });
        }
        stop.store(true, Ordering::Release);
        assert_eq!(h.join().unwrap(), 10);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, 64 + 10 * 64, "header + ten slots");
        std::fs::remove_dir_all(&dir).unwrap();

        let (p, _c) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
        let mut tx = JournalTx::new(p);
        for _ in 0..JOURNAL_RING + 5 {
            tx.note(&ExecRecord::default());
        }
        assert_eq!(tx.dropped(), 5);
    }

    /// The anchor file is one session's (BX7, N2): this key's and this
    /// account mode's. Another key's, another mode's or an unreadable line
    /// refuses — never inherited, never silently replaced; a line written
    /// before BX7 is a classic session's. Break-and-watch: without the mode
    /// check a Portfolio Margin boot reads the classic session's anchor.
    #[test]
    fn the_anchor_is_one_sessions_own() {
        let dir = std::env::temp_dir().join(format!("bn-anchor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("binance-pnl-anchor.state");
        let a = AnchorFile::new(path.clone(), "KEYAAAAAAAAAAAAAAAA", AccountMode::Classic);
        assert_eq!(a.load(), Ok(None), "no file: no session open");
        a.store(1_010_250_000).unwrap();
        assert_eq!(a.load(), Ok(Some(1_010_250_000)));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("KEYAAAA"), "the key itself is never written");
        assert!(text.ends_with("\tclassic\n"), "{text}");
        let other = AnchorFile::new(path.clone(), "KEYBBBBBBBBBBBBBBBB", AccountMode::Classic);
        let e = other.load().unwrap_err();
        assert!(e.contains("another API key") && !e.contains("KEY"), "{e}");
        let pm = AnchorFile::new(path.clone(), "KEYAAAAAAAAAAAAAAAA", AccountMode::Pm);
        let e = pm.load().unwrap_err();
        assert!(e.contains("`classic` session") && e.contains("`pm`"), "{e}");
        // A line written before BX7: a classic session's.
        let tag = text.split('\t').next().unwrap();
        std::fs::write(&path, format!("{tag}\t5000000\t1790000000\n")).unwrap();
        assert_eq!(a.load(), Ok(Some(5_000_000)));
        assert!(pm.load().is_err());
        let (zero, five) = (format!("{tag}\t0\t1"), format!("{tag}\t5\t1\tpm_pro\tx"));
        for bad in ["garbage", "", "\t5000000\t1", zero.as_str(), five.as_str()] {
            std::fs::write(&path, bad).unwrap();
            assert!(a.load().unwrap_err().contains("not an anchor line"), "{bad:?}");
        }
        std::fs::write(&path, format!("{tag}\t5\t1\tportfolio\n")).unwrap();
        assert!(a.load().unwrap_err().contains("`unknown` session"));
        // A file past the longest line is refused, never read whole.
        std::fs::write(&path, format!("{tag}\t5000000\t1790000000\tclassic{}\n", " ".repeat(200))).unwrap();
        assert!(a.load().unwrap_err().contains("not an anchor line"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// F2: an anchor the writer cannot store raises the flag and is tried
    /// again; once the store works it lands and the flag drops.
    /// Break-and-watch: `let _ = f.store(..)` (the old writer) never
    /// raises the flag.
    #[test]
    fn an_anchor_that_cannot_be_stored_is_flagged_and_retried() {
        let dir = std::env::temp_dir().join(format!("bn-anchor-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blocked = dir.join("not-yet");
        // A FILE where the anchor's directory must be: every store fails.
        std::fs::write(&blocked, b"x").unwrap();
        let anchor = AnchorFile::new(blocked.join("binance-pnl-anchor.state"), "KEYCCCCCCCCCCCCCCCC", AccountMode::Classic);
        let flag = anchor.unsaved_flag();
        let (p, c) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
        let mut tx = JournalTx::new(p);
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_writer(&dir.join("binance-exec.pmlr"), 1, c, stop.clone(), Some(anchor.clone())).unwrap();
        assert!(tx.note(&ExecRecord { kind: J_ANCHOR, a: 1_000_000_000, ..ExecRecord::default() }));
        let t0 = std::time::Instant::now();
        while !flag.load(Ordering::Acquire) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5), "the failed store is flagged");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The directory appears: the next retry lands it.
        std::fs::remove_file(&blocked).unwrap();
        std::fs::create_dir_all(&blocked).unwrap();
        while flag.load(Ordering::Acquire) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5), "the retry stores it");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(anchor.load(), Ok(Some(1_000_000_000)));
        stop.store(true, Ordering::Release);
        h.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
