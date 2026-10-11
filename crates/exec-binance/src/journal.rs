// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The `binance-exec.pmlr` journal (plan §3.13; BX-14).**
//!
//! What the venue said, in order, for the fee measurement (BX-14) and the
//! shadow reconciliation tool: ACKs, refusals with their code, fills with
//! their maker flag, each trade's commission with its asset ([`J_FEE`],
//! BX11), cancels and expiries with their reason, reconciliation verdicts
//! and margin samples — one [`core_types::ExecRecord`] (64 B) each.
//!
//! **Out of band.** The gateway never touches the disk: it pushes records
//! into an SPSC ring ([`JournalTx::note`]) and a cold thread
//! ([`spawn_writer`]) appends them to the file and flushes once a second.
//! A full ring DROPS the record and counts it — the journal is evidence,
//! never a brake on the order path. The ring's capacity is the type's
//! (BX11): the engine's is [`JOURNAL_RING`]; a test's can be two.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core_io::pmlr::SlotKind;
use core_io::slot_capture::SlotCapture;
use core_ring::{Consumer, Producer};
use core_types::ExecRecord;

use crate::inst::{BnInstWire, INST_DATED, INST_INVERSE, INST_TRADFI, PRODUCT_EQUITY, PRODUCT_OPTIONS};
use crate::mode::AccountMode;

/// The ring between the gateway and the writer.
pub const JOURNAL_RING: usize = 1_024;

/// An ACK: `a` = 0.
pub const J_ACK: u8 = 1;
/// A refusal: `code`.
pub const J_REJECT: u8 = 2;
/// A fill: `a` = price ×1e6, `b` = quantity ×1e6 (signed by side), `c` =
/// 0 (BX11: the trade's commission is its [`J_FEE`]'s — through BX7b, the
/// update's ×1e6 when the update booked first); `code` = 1 if the fill was
/// a maker, plus [`J_FILL_LOST`] if it never reached lane 4.
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
/// **A trade's commission** (BX11; BX-14's one source). `client_oid` the
/// member's id (as [`J_FILL`]'s), `venue_oid` the venue's order id, `a` =
/// the trade's price ×1e6, `b` = its notional ×1e9 in the row's settlement
/// asset ([`fee_notional_1e9`]), `c` = the commission ×1e9 in its own asset
/// (`n`; negative for a rebate); `code` = [`fee_code`]'s bits. Only the
/// `ORDER_TRADE_UPDATE` carries a commission, and `TRADE_LITE` usually
/// books the fill first: the record is journaled once per trade, from the
/// first update that reports it, whichever report booked the fill
/// (`crate::oot::TidRing`). A trade the stream lost has none; neither has
/// a fill booked without its trade ([`J_FILL_UNSEEN`], [`J_FILL_ADOPTED`]).
pub const J_FEE: u8 = 10;
/// **BX10 — the broker accepted an equity order** (Binance Stocks'
/// `ACCEPTED`: its own acknowledgement, after the venue's receipt — the
/// [`J_ACK`]): `venue_oid` the order id's key (`crate::equity::eq_key` —
/// the UUID itself is not a number), `a` = 0. Once per order.
pub const J_ACCEPTED: u8 = 11;

/// [`J_FEE`] `code` bits 1–2, the commission's asset to its row
/// ([`fee_asset`]): `N` absent.
pub const FEE_ASSET_NONE: u8 = 0;
/// The row's settlement asset — the one its notional ([`J_FEE`]'s `b`) is
/// in: BX-14's rate is `c / b`.
pub const FEE_ASSET_SETTLE: u8 = 1;
/// BNB: the BNB burn is on (the plan's account precondition 6 says off).
pub const FEE_ASSET_BNB: u8 = 2;
/// Another asset.
pub const FEE_ASSET_OTHER: u8 = 3;

/// [`J_FEE`] `code` bits 4–6, whose order traded: the member's.
pub const TRADE_BY_MEMBER: u8 = 0;
/// An arm's id no member holds (an earlier boot's order).
pub const TRADE_BY_ORPHAN: u8 = 1;
/// The venue's settlement (a delivery, a delisting's close).
pub const TRADE_BY_SETTLEMENT: u8 = 2;
/// A liquidation.
pub const TRADE_BY_LIQUIDATION: u8 = 3;
/// An auto-deleveraging.
pub const TRADE_BY_ADL: u8 = 4;

/// [`J_FEE`] `code` bit 14 (BX10, BX-14): a Binance Stocks order's fee is
/// at the platform's per-order minimum ($0.35 — the fee is the greater of
/// it and 10 bps, so every order under $350 of notional pays the floor,
/// not the rate): its `c / b` is no rate; a measurement excludes it.
pub const FEE_FLAG_EQ_FLOOR: i32 = 1 << 14;
/// The platform's per-order minimum fee, USD ×1e6.
pub const EQ_FEE_MIN_USD_1E6: i64 = 350_000;
/// The notional under which the minimum applies (10 bps of it is the
/// minimum), USD ×1e6.
pub const EQ_FEE_FLOOR_NOTIONAL_1E6: i64 = 350_000_000;

/// **[`J_FEE`]'s `code`**: bit 0 the trade was a maker · bits 1–2 the
/// asset (`FEE_ASSET_*`) · bits 4–6 whose order (`TRADE_BY_*`) · bits 8–11
/// the row's product (`PRODUCT_*`) · bit 12 a dated row · bit 13 a TradFi
/// row — the fee classes BX-14 measures, readable without the boot's
/// instrument table.
#[inline]
#[must_use]
pub const fn fee_code(maker: bool, asset: u8, by: u8, w: &BnInstWire) -> i32 {
    (maker as i32)
        | ((asset as i32 & 3) << 1)
        | ((by as i32 & 7) << 4)
        | ((w.product as i32 & 15) << 8)
        | (((w.flags & INST_DATED != 0) as i32) << 12)
        | (((w.flags & INST_TRADFI != 0) as i32) << 13)
}

/// [`fee_code`]'s fields back: (maker, asset, by, product, dated, TradFi).
#[must_use]
pub const fn fee_fields(code: i32) -> (bool, u8, u8, u8, bool, bool) {
    (
        code & 1 != 0,
        ((code >> 1) & 3) as u8,
        ((code >> 4) & 7) as u8,
        ((code >> 8) & 15) as u8,
        code & (1 << 12) != 0,
        code & (1 << 13) != 0,
    )
}

/// **The commission's asset `asset` (`N`) to row `w`**: its settlement
/// asset — USDⓈ-M's quote, USDT or USDC, whole (the symbol, before any `_`
/// expiry, ends with it: `BTCUSDT`, `BTCUSDC`, `BTCUSDT_261225`; a part of
/// it, `T` or `SDT`, is another asset), COIN-M's margin coin (the symbol
/// is the coin then `USD`: `BTCUSD_PERP`), an option's USDT (BX9) — or
/// BNB, another, none.
#[must_use]
pub fn fee_asset(w: &BnInstWire, asset: &[u8]) -> u8 {
    if asset.is_empty() {
        return FEE_ASSET_NONE;
    }
    let sym = w.symbol();
    let mut n = 0;
    while n < sym.len() && sym[n] != b'_' {
        n += 1;
    }
    let head = &sym[..n];
    let settles = if w.flags & INST_INVERSE != 0 {
        head.len() == asset.len() + 3 && head.starts_with(asset) && head.ends_with(b"USD")
    } else if w.product == PRODUCT_OPTIONS {
        // BX9: options settle in USDT alone (`optionAssets`); the symbol
        // (`BTC-261030-60000-C`) names no quote.
        asset == b"USDT"
    } else if w.product == PRODUCT_EQUITY {
        // BX10: Binance Stocks' fee is in USD (the detail's `fee`); the
        // ticker names no quote.
        asset == b"USD"
    } else {
        (asset == b"USDT" || asset == b"USDC") && head.len() > asset.len() && head.ends_with(asset)
    };
    if settles {
        FEE_ASSET_SETTLE
    } else if asset == b"BNB" {
        FEE_ASSET_BNB
    } else {
        FEE_ASSET_OTHER
    }
}

/// **A trade's notional ×1e9 in row `w`'s settlement asset** ([`J_FEE`]'s
/// `b`): linear, price × quantity (the quote); inverse (COIN-M), contracts
/// × the face ÷ the price (the coin). 0 for a price ≤ 0 (no rate is read
/// from it); `i64::MAX` past the range.
#[must_use]
pub fn fee_notional_1e9(w: &BnInstWire, px_1e6: i64, qty_1e6: i64) -> i64 {
    if px_1e6 <= 0 {
        return 0;
    }
    let q = i128::from(qty_1e6.saturating_abs());
    let v = if w.flags & INST_INVERSE != 0 {
        // A face past any venue's (discovery takes up to `i64::MAX`)
        // saturates, never wraps.
        match q.checked_mul(i128::from(w.unit_1e6)).and_then(|x| x.checked_mul(1_000)) {
            Some(x) => x / i128::from(px_1e6),
            None => return i64::MAX,
        }
    } else {
        q * i128::from(px_1e6) / 1_000
    };
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The gateway's end, over a ring of `N` records (a power of two ≥ 2).
pub struct JournalTx<const N: usize = JOURNAL_RING> {
    p: Producer<ExecRecord, N>,
    dropped: u64,
}

impl<const N: usize> JournalTx<N> {
    /// Over the ring's producer.
    #[must_use]
    pub const fn new(p: Producer<ExecRecord, N>) -> Self {
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
pub fn spawn_writer<const N: usize>(
    path: &Path,
    epoch_ns: u64,
    mut rx: Consumer<ExecRecord, N>,
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

/// The longest anchor file read (BX7): `store` writes ≤ 96 B; a longer
/// file is no anchor line, refused unread past this.
const ANCHOR_READ_MAX: u64 = 128;

/// The longest equity basis (BX9): `um+opt-` and an options key's tag.
const BASIS_MAX: usize = 24;

/// A key's tag: the first 8 bytes of SHA-256 of the API key, in hex —
/// never the key.
fn key_tag(api_key: &str) -> [u8; 16] {
    let h = core_crypto::sha256(api_key.as_bytes());
    let hex = b"0123456789abcdef";
    let mut tag = [0u8; 16];
    let mut i = 0;
    while i < 8 {
        tag[2 * i] = hex[(h[i] >> 4) as usize];
        tag[2 * i + 1] = hex[(h[i] & 0x0f) as usize];
        i += 1;
    }
    tag
}

/// A basis without its options key's tag (`um+opt-<tag>` → `um+opt`): the
/// words a refusal prints — the tag never.
fn basis_parts(basis: &str) -> &str {
    basis.split('-').next().unwrap_or("")
}

/// **The E7 session anchor, persisted** (`binance-pnl-anchor.state`).
///
/// The session bound is judged from the account's equity at the first
/// reconciliation of the SESSION, and a session outlives a process (the
/// restart lane relaunches the engine): the anchor is written once and read
/// at every boot; the operator ends a session by deleting the file. One
/// line: `<account tag>\t<equity ×1e6>\t<unix s>\t<account mode>\t<basis>`.
/// The tag is the first 8 bytes of SHA-256 of the API key, in hex — never
/// the key.
///
/// The file is ONE session's (BX7, the risk review's N2): the equity's
/// basis is the account mode's — USDⓈ-M's margin balance on a classic
/// account, the unified account's in PM and PM Pro — so a boot under
/// another key or in another mode is refused until the operator ends that
/// session: never inherited, never silently replaced (which would forget
/// its losses). `signing = "hmac"` signs with another key: toggling it ends
/// the session the same way. A line written before BX7 names no mode — a
/// classic session's.
///
/// **The basis (BX9, Q-4; the risk review's R1):** a classic session's
/// equity is a SUM of the parts armed — USDⓈ-M's margin balance (`um`),
/// the options account's equity (`opt-<the options key's tag>`), or both
/// (`um+opt-<tag>`); PM and PM Pro read one unified equity (`unified`).
/// Arming options into a USDⓈ-M session (or dropping them, or another
/// options key) changes the sum without a trade: the boot refuses — "delete
/// it to end that session" — as for another mode. A line written before
/// BX9 names no basis: its mode's alone (`um` classic, `unified` PM).
/// Boot and one cold write only.
#[derive(Clone, Debug)]
pub struct AnchorFile {
    path: std::path::PathBuf,
    tag: [u8; 16],
    /// The account mode the session's equity is read in.
    mode: AccountMode,
    /// The equity's basis (see the type's docs): `basis[..basis_len]`.
    basis: [u8; BASIS_MAX],
    /// See [`Self::basis`].
    basis_len: u8,
    /// The anchor could not be stored (yet): raised by the writer while
    /// its store fails, read by the gateway's pulse (F2).
    unsaved: Arc<AtomicBool>,
}

impl AnchorFile {
    /// The file at `path`, for the account whose API key is `api_key`, in
    /// account mode `mode` — its basis the mode's alone (`um` classic,
    /// `unified` PM / PM Pro) until [`Self::with_options`].
    #[must_use]
    pub fn new(path: std::path::PathBuf, api_key: &str, mode: AccountMode) -> Self {
        let mut f = Self {
            path,
            tag: key_tag(api_key),
            mode,
            basis: [0; BASIS_MAX],
            basis_len: 0,
            unsaved: Arc::new(AtomicBool::new(false)),
        };
        f.set_basis(Self::mode_basis(mode).as_bytes());
        f
    }

    /// **BX9 (Q-4)**: the session's equity sums the options account's,
    /// read with the key `opt_api_key` — with USDⓈ-M's when `usdm`.
    #[must_use]
    pub fn with_options(mut self, usdm: bool, opt_api_key: &str) -> Self {
        let tag = key_tag(opt_api_key);
        let head: &[u8] = if usdm { b"um+opt-" } else { b"opt-" };
        let n = head.len() + tag.len();
        debug_assert!(n <= BASIS_MAX);
        // COPY: the basis' head (≤ 7 B) and the key's tag (16 B) straight
        // into the anchor's own field — boot only, once; rejected: two
        // fields (the file's line and the comparison take the basis as one
        // word).
        self.basis[..head.len()].copy_from_slice(head);
        self.basis[head.len()..n].copy_from_slice(&tag);
        self.basis_len = n as u8;
        self
    }

    /// The basis a line naming none was written in: its mode's alone.
    const fn mode_basis(mode: AccountMode) -> &'static str {
        match mode {
            AccountMode::Classic => "um",
            AccountMode::Pm | AccountMode::PmPro => "unified",
        }
    }

    fn set_basis(&mut self, b: &[u8]) {
        debug_assert!(b.len() <= BASIS_MAX);
        // COPY: the basis (≤ `BASIS_MAX` 24 B) into the anchor's own field —
        // boot only, once; rejected: a borrowed `&'static str` (the tag is
        // the key's, computed at boot).
        self.basis[..b.len()].copy_from_slice(b);
        self.basis_len = b.len() as u8;
    }

    fn basis(&self) -> &str {
        core::str::from_utf8(&self.basis[..usize::from(self.basis_len)]).unwrap_or("")
    }

    /// The "could not be stored" flag, shared with the gateway (F2).
    #[must_use]
    pub fn unsaved_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.unsaved)
    }

    /// The session's anchor (boot): `None` when no session is open (no
    /// file). `Err` — the boot refuses, naming the remedy and never the key
    /// — when the file is another key's session, another account mode's or
    /// another basis's, or cannot be read as one.
    ///
    /// # Errors
    /// The file is not this session's, or not an anchor.
    pub fn load(&self) -> Result<Option<i64>, String> {
        // COPY: the anchor file (one line: ≤ 96 B as `store` writes it; a
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
        let (tag, v, unix, mode, basis, extra) = (it.next(), it.next(), it.next(), it.next(), it.next(), it.next());
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
        // A line written before BX9 names no basis: its mode's alone.
        let theirs = basis.unwrap_or(Self::mode_basis(self.mode));
        if theirs != self.basis() {
            let (t, o) = (basis_parts(theirs), basis_parts(self.basis()));
            return Err(format!(
                "holds a session whose equity is `{t}` and this boot's is `{o}`{} — the equity's \
                 basis differs (BX9: options summed or not); delete it to end that session",
                if t == o { " under another options key" } else { "" }
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
        core_io::write_atomic(
            &self.path,
            &format!("{tag}\t{equity_1e6}\t{unix}\t{}\t{}\n", self.mode.word(), self.basis()),
        )
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
        assert!(text.ends_with("\tclassic\tum\n"), "{text}");
        let other = AnchorFile::new(path.clone(), "KEYBBBBBBBBBBBBBBBB", AccountMode::Classic);
        let e = other.load().unwrap_err();
        assert!(e.contains("another API key") && !e.contains("KEY"), "{e}");
        let pm = AnchorFile::new(path.clone(), "KEYAAAAAAAAAAAAAAAA", AccountMode::Pm);
        let e = pm.load().unwrap_err();
        assert!(e.contains("`classic` session") && e.contains("`pm`"), "{e}");
        // A line written before BX7: a classic session's; before BX9: its
        // mode's basis alone.
        let tag = text.split('\t').next().unwrap();
        std::fs::write(&path, format!("{tag}\t5000000\t1790000000\n")).unwrap();
        assert_eq!(a.load(), Ok(Some(5_000_000)));
        assert!(pm.load().is_err());
        std::fs::write(&path, format!("{tag}\t5000000\t1790000000\tpm\n")).unwrap();
        assert_eq!(pm.load(), Ok(Some(5_000_000)), "a PM line before BX9: unified");
        let (zero, six) = (format!("{tag}\t0\t1"), format!("{tag}\t5\t1\tpm_pro\tunified\tx"));
        for bad in ["garbage", "", "\t5000000\t1", zero.as_str(), six.as_str()] {
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

    /// BX9 (Q-4, the risk review's R1): the anchor names the equity's
    /// basis — the parts summed and the options key's tag. Arming options
    /// into a USDⓈ-M session, dropping them, or another options key refuses
    /// the boot; the tag is written, never the key, and a refusal prints
    /// the parts alone. Break-and-watch: without the basis check a
    /// `um+opt` boot reads a `um` session's anchor and the options
    /// account's equity reads as that session's gain.
    #[test]
    fn the_anchor_records_its_equity_basis() {
        let dir = std::env::temp_dir().join(format!("bn-anchor-basis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("binance-pnl-anchor.state");
        let key = "KEYAAAAAAAAAAAAAAAA";
        let um = AnchorFile::new(path.clone(), key, AccountMode::Classic);
        let both = AnchorFile::new(path.clone(), key, AccountMode::Classic).with_options(true, "OPTKEYOOOOOOOOOOOOO");
        let opt = AnchorFile::new(path.clone(), key, AccountMode::Classic).with_options(false, "OPTKEYOOOOOOOOOOOOO");
        let other = AnchorFile::new(path.clone(), key, AccountMode::Classic).with_options(true, "OPTKEYPPPPPPPPPPPPP");
        um.store(2_000_000_000).unwrap();
        let e = both.load().unwrap_err();
        assert!(e.contains("`um` and this boot's is `um+opt`") && e.contains("delete it"), "{e}");
        both.store(3_000_000_000).unwrap();
        assert_eq!(both.load(), Ok(Some(3_000_000_000)));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("OPTKEY") && text.contains("\tclassic\tum+opt-"), "{text}");
        assert!(um.load().unwrap_err().contains("`um+opt` and this boot's is `um`"));
        assert!(opt.load().unwrap_err().contains("this boot's is `opt`"));
        let e = other.load().unwrap_err();
        assert!(e.contains("under another options key") && !e.contains(&text[..16]), "{e}");
        // A line before BX9 (no basis): a classic session's USDⓈ-M alone.
        let tag = text.split('\t').next().unwrap();
        std::fs::write(&path, format!("{tag}\t5000000\t1790000000\tclassic\n")).unwrap();
        assert_eq!(um.load(), Ok(Some(5_000_000)));
        assert!(both.load().is_err());
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

    const FEE_FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]},{"symbol":"BTCUSDC","pair":"BTCUSDC","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDC","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]},{"symbol":"BTCUSDT_261225","pair":"BTCUSDT","contractType":"CURRENT_QUARTER","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]},{"symbol":"TSLAUSDT","pair":"TSLAUSDT","contractType":"TRADIFI_PERPETUAL","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]}]}"#;
    const FEE_DAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","contractStatus":"TRADING","marginAsset":"BTC","contractSize":100,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.1"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]},{"symbol":"BNBUSD_PERP","pair":"BNBUSD","contractType":"PERPETUAL","contractStatus":"TRADING","marginAsset":"BNB","contractSize":10,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.001"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]},{"symbol":"BTCEUR_PERP","pair":"BTCEUR","contractType":"PERPETUAL","contractStatus":"TRADING","marginAsset":"BTC","contractSize":100,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.1"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]}]}"#;

    /// The wire rows: BTCUSDT, BTCUSDC, BTCUSDT_261225, TSLAUSDT (USDⓈ-M),
    /// BTCUSD_PERP, BNBUSD_PERP and a made-up BTCEUR_PERP (COIN-M) — rows 0
    /// to 6.
    fn fee_rows() -> crate::inst::WireTable {
        let (fapi, dapi) = (crate::inst::tests::fapi_rows(FEE_FAPI), crate::inst::tests::fapi_rows(FEE_DAPI));
        let (mut t, mut w) = crate::inst::InstTable::new(7);
        let rows: [(&[u8], u8); 7] = [
            (b"BTCUSDT", crate::inst::PRODUCT_USDM),
            (b"BTCUSDC", crate::inst::PRODUCT_USDM),
            (b"BTCUSDT_261225", crate::inst::PRODUCT_USDM),
            (b"TSLAUSDT", crate::inst::PRODUCT_USDM),
            (b"BTCUSD_PERP", crate::inst::PRODUCT_COINM),
            (b"BNBUSD_PERP", crate::inst::PRODUCT_COINM),
            (b"BTCEUR_PERP", crate::inst::PRODUCT_COINM),
        ];
        let mut i = 0;
        while i < rows.len() {
            let (name, product) = rows[i];
            let d = if product == crate::inst::PRODUCT_USDM { &fapi } else { &dapi };
            let spec = crate::inst::BindSpec {
                sym: core_types::make_symbol_id(core_types::VenueId::Binance, 700 + i as u32),
                product,
                row: d.find(name).expect("a fixture row"),
                owned: true,
                maker_ok: true,
            };
            assert_eq!(t.bind(&mut w, &spec), Ok(i as u16));
            i += 1;
        }
        w
    }

    /// BX11: a commission's asset is judged against its row — USDⓈ-M's
    /// quote (a USDC row's, a dated row's, a TradFi row's), COIN-M's coin —
    /// with BNB and the rest named apart; a row is never "settled" in an
    /// asset its symbol merely contains. Break-and-watch: dropping the
    /// inverse arm's `USD` tail check (or comparing the whole symbol)
    /// misjudges a row below.
    #[test]
    fn a_commissions_asset_is_judged_against_its_row() {
        let w = fee_rows();
        let cases: [(u16, &[u8], u8); 21] = [
            (0, b"USDT", FEE_ASSET_SETTLE),
            // The parser review's P-6: a tail of the quote is not the quote.
            (0, b"T", FEE_ASSET_OTHER),
            (0, b"DT", FEE_ASSET_OTHER),
            (0, b"SDT", FEE_ASSET_OTHER),
            (1, b"C", FEE_ASSET_OTHER),
            (0, b"BNB", FEE_ASSET_BNB),
            (0, b"USDC", FEE_ASSET_OTHER),
            (0, b"BTC", FEE_ASSET_OTHER),
            (0, b"", FEE_ASSET_NONE),
            (0, b"BTCUSDT", FEE_ASSET_OTHER),
            (1, b"USDC", FEE_ASSET_SETTLE),
            (1, b"USDT", FEE_ASSET_OTHER),
            (2, b"USDT", FEE_ASSET_SETTLE),
            (3, b"USDT", FEE_ASSET_SETTLE),
            (4, b"BTC", FEE_ASSET_SETTLE),
            (4, b"BNB", FEE_ASSET_BNB),
            (4, b"USD", FEE_ASSET_OTHER),
            (4, b"BT", FEE_ASSET_OTHER),
            (5, b"BNB", FEE_ASSET_SETTLE),
            (5, b"BTC", FEE_ASSET_OTHER),
            (6, b"BTC", FEE_ASSET_OTHER),
        ];
        let mut i = 0;
        while i < cases.len() {
            let (row, asset, want) = cases[i];
            assert_eq!(fee_asset(w.row(row), asset), want, "{:?} on row {row}", core::str::from_utf8(asset));
            i += 1;
        }
        // BX10: an equity row's fee is in USD, its settlement asset.
        let rules = crate::equity::scan_eq_symbol(
            br#"{"symbols":[{"symbol":"AAPL","tradability":"BUY_SELL","fractionable":true,"stepSize":"0.000000001","minNotional":"5"}]}"#,
            b"AAPL",
        )
        .expect("the rules");
        let (mut t, mut we) = crate::inst::InstTable::new(7);
        let r = t
            .bind_equity(&mut we, &crate::inst::EqBindSpec { sym: core_types::make_symbol_id(core_types::VenueId::Binance, 4_097), row: &rules, owned: true, bstock: b"" })
            .expect("bound");
        assert_eq!((fee_asset(we.row(r), b"USD"), fee_asset(we.row(r), b"USDC"), fee_asset(we.row(r), b"")), (FEE_ASSET_SETTLE, FEE_ASSET_OTHER, FEE_ASSET_NONE));
    }

    /// BX11: the notional is the quote's (price × quantity) on a linear
    /// row and the coin's (contracts × face ÷ price) on an inverse one,
    /// ×1e9 either way; the code carries the fee class and reads back.
    #[test]
    fn a_fees_notional_and_class() {
        let w = fee_rows();
        // 0.002 BTC at 65 000 = 130 USDT; 3 contracts × 100 USD at 65 000 =
        // 0.004615384 BTC; 7 contracts × 10 USD at 600 = 0.116666666 BNB.
        assert_eq!(fee_notional_1e9(w.row(0), 65_000_000_000, 2_000), 130_000_000_000);
        assert_eq!(fee_notional_1e9(w.row(0), 65_000_000_000, -2_000), 130_000_000_000, "the size, whichever side");
        assert_eq!(fee_notional_1e9(w.row(4), 65_000_000_000, 3_000_000), 4_615_384);
        assert_eq!(fee_notional_1e9(w.row(5), 600_000_000, 7_000_000), 116_666_666);
        assert_eq!(fee_notional_1e9(w.row(4), 0, 3_000_000), 0, "no price, no notional");
        assert_eq!(fee_notional_1e9(w.row(0), i64::MAX, i64::MAX), i64::MAX, "saturated, never wrapped");
        // The parser review's P-7: an absurd face saturates (it overflowed
        // i128 and panicked in a debug build).
        let mut huge = *w.row(4);
        huge.unit_1e6 = i64::MAX;
        assert_eq!(fee_notional_1e9(&huge, 1, i64::MAX), i64::MAX, "saturated, never wrapped");
        let code = fee_code(true, FEE_ASSET_SETTLE, TRADE_BY_ORPHAN, w.row(3));
        assert_eq!(fee_fields(code), (true, FEE_ASSET_SETTLE, TRADE_BY_ORPHAN, crate::inst::PRODUCT_USDM, false, true));
        let code = fee_code(false, FEE_ASSET_BNB, TRADE_BY_ADL, w.row(2));
        assert_eq!(fee_fields(code), (false, FEE_ASSET_BNB, TRADE_BY_ADL, crate::inst::PRODUCT_USDM, true, false));
        let code = fee_code(false, FEE_ASSET_OTHER, TRADE_BY_LIQUIDATION, w.row(4));
        assert_eq!(fee_fields(code), (false, FEE_ASSET_OTHER, TRADE_BY_LIQUIDATION, crate::inst::PRODUCT_COINM, false, false));
    }
}
