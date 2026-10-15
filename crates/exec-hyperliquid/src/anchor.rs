// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! E7 — the SESSION-BOUND anchor.
//!
//! Operator ruling 2026-09-19: *"run until it either earns +15 USDC or
//! loses 5 USDC."* The bound itself is a sticky halt the router judges
//! (`exec_router::halt::trigger_for`, reasons `pnl-gain` / `pnl-loss`)
//! from `HaltSignal::pnl_delta_usd_1e6`; this module owns only the
//! number the delta is measured FROM: the account's spot USDC at the
//! first FLAT reconciliation of the session.
//!
//! Why persisted: the launchd `KeepAlive` relaunches the engine within
//! a minute of any exit, and every restart resets every per-process
//! number. A bound that re-anchored on each boot would be a bound on
//! nothing — a session that lost $4, restarted and lost $4 again would
//! never halt. The file makes "the session" outlive the process; the
//! operator ENDS a session by deleting the file before a restart, and
//! by nothing else.
//!
//! Why spot USDC and why only when flat: an outcome leg is worth
//! anything from 0 to 1 USDC until the venue settles it, so an account
//! holding one has no P&L to read — the premium it paid is not a loss
//! and the payout it may get is not a gain. The moment it holds NO leg,
//! the USDC row is the whole account and its change since the anchor is
//! exactly the session's realised result, fees included, from the one
//! source the engine already trusts over its own ledger (the
//! reconciler's `spotClearinghouseState`).
//!
//! One line, tab-separated: `<0x master hex>\t<usdc ×1e6>\t<unix s set>`.
//! A line for another address is NOT an anchor — the budget file has
//! the same rule for the same reason: inheriting another account's
//! number is exactly the mistake.
//!
//! BOOT/OFFLINE DOCTRINE: [`load`] and [`load_own`] run once at boot;
//! [`store`] runs once per session — from the blocking arm's reconciler,
//! or from slot 6's [`AnchorKeeper`] on its gateway's thread. Allocation
//! is fine in those, and none is on the tick path. [`AnchorSlot`]'s
//! methods (the engine thread, every pass until kept) and the keeper's
//! idle pass (the gateway thread, every turn) are one atomic each, 0 B.
//!
//! **XMM XH5b-2 — slot 6's own account** keeps its anchor the same way, in
//! a file of its own beside the exec artifact, one a network
//! (`xmm-pnl-anchor-<network>.state`; the number is the account's worth,
//! XH5b-1): read strictly at boot ([`load_own`]: anything but this
//! master's line refuses the boot), and a fresh one handed over by the
//! gateway arm through an [`AnchorSlot`] and stored by an [`AnchorKeeper`]
//! on the gateway's thread — off the engine thread, and before the slot's
//! first quote (the arm reports itself reconciled only once it reads the
//! anchor kept). A store that fails halts the gateway
//! ([`HALT_ANCHOR_UNKEPT`]).

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use exec_gateway::GatewayStats;

use crate::gateway::HALT_ANCHOR_UNKEPT;

/// Default location of the anchor file — beside the budget's.
pub const DEFAULT_STATE_PATH: &str = "exec-pnl-anchor.state";

/// The anchor: whose, how much, and when it was set.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PnlAnchor {
    /// The MASTER account the balance belongs to.
    pub address: [u8; 20],
    /// Spot USDC at the first flat reconciliation, ×1e6 (slot 6's own
    /// account: the account's worth, XMM XH5b-1). Always > 0: an account
    /// with no USDC has nothing to bound, and `0` is the in-memory
    /// spelling of "not anchored yet".
    pub usdc_1e6: i64,
    /// Unix seconds when it was set — for the operator, never read
    /// back by the engine.
    pub set_unix_s: u64,
}

impl PnlAnchor {
    /// The state-file line, newline-terminated. By value: the struct
    /// is `Copy` and 40 B, and this runs once per session.
    #[must_use]
    pub fn to_line(self) -> String {
        format!(
            "{}\t{}\t{}\n",
            crate::config::hex20(&self.address),
            self.usdc_1e6,
            self.set_unix_s
        )
    }

    /// Restore from a state-file line, for `address`.
    ///
    /// `None` — meaning "not anchored; anchor at the next flat
    /// reconciliation" — for a malformed line, a non-positive balance,
    /// or a line written for a different address.
    #[must_use]
    pub fn from_line(line: &str, address: [u8; 20]) -> Option<Self> {
        let mut f = line.trim().split('\t');
        let addr_s = f.next()?;
        let usdc_1e6: i64 = f.next()?.parse().ok()?;
        if usdc_1e6 <= 0 {
            return None;
        }
        let set_unix_s: u64 = f.next()?.parse().ok()?;
        if f.next().is_some() {
            return None;
        }
        if !addr_s.eq_ignore_ascii_case(&crate::config::hex20(&address)) {
            return None;
        }
        Some(Self {
            address,
            usdc_1e6,
            set_unix_s,
        })
    }
}

/// Read the anchor for `address` from `path`.
///
/// Every failure is `None` — a missing file is the ordinary first boot
/// of a session, and every other failure reads the same way because
/// the only safe answer to "we do not know where this session started"
/// is to start it here, at the next flat reconciliation.
#[must_use]
pub fn load(path: &std::path::Path, address: [u8; 20]) -> Option<PnlAnchor> {
    // COPY: one ≤ 100 B state line read into a String at BOOT — the
    // restore path, once per process; never the tick path.
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| PnlAnchor::from_line(&t, address))
}

/// Write the anchor durably (temp file, `sync_all`, rename).
///
/// Called once per session — the blocking arm's reconciler, or slot 6's
/// [`AnchorKeeper`] — never the tick path.
///
/// # Errors
/// The temp write, the fsync or the rename failed; the message names
/// the path.
pub fn store(path: &std::path::Path, a: PnlAnchor) -> Result<(), String> {
    core_io::write_atomic(path, &a.to_line())
}

/// **XMM XH5b-2 — the strict read, for slot 6's own account.** `Ok(None)`
/// only when there is no file: a session's first boot. The anchor when the
/// file holds this master's line. **An error otherwise** — a file that
/// cannot be read, a malformed line, another master's session not ended:
/// the boot refuses rather than re-anchor over a session it cannot see (a
/// re-anchored bound is a bound on nothing), and only the operator ends a
/// session, by moving the file aside.
///
/// # Errors
/// As above, the path and the master named.
pub fn load_own(path: &std::path::Path, address: [u8; 20]) -> Result<Option<PnlAnchor>, String> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut buf = [0u8; LINE_MAX + 1];
    let mut n = 0usize;
    while n < buf.len() {
        // COPY: the state file into a 129 B stack buffer at BOOT, once — a
        // file read is a copy; rejected: an unbounded `read_to_string` (a
        // file of any size), a mapping for ≤ 84 B.
        match std::io::Read::read(&mut f, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    let line = if n <= LINE_MAX {
        core::str::from_utf8(&buf[..n]).ok()
    } else {
        None
    };
    line.and_then(|t| PnlAnchor::from_line(t, address))
        .map(Some)
        .ok_or_else(|| {
            format!(
                "{}: not a session anchor of {} (a malformed or overlong line, or another \
                 master's session not ended) — end that session by moving the file aside, then \
                 boot again (XMM XH5b-2)",
                path.display(),
                crate::config::hex20(&address)
            )
        })
}

/// The longest line [`load_own`] reads: a well-formed one is ≤ 84 B (`0x`
/// and 40 hex, two tabs, two decimals of ≤ 20 digits, a newline).
const LINE_MAX: usize = 128;

/// **XMM XH5b-2 — a fresh anchor's hand-over** from slot 6's gateway arm
/// (the engine thread) to its [`AnchorKeeper`] (the gateway's thread), one
/// word: 0 — nothing to keep; > 0 — an anchor (USD ×1e6) to store; < 0 —
/// stored (its negation). The arm writes an anchor once a boot and reads it
/// back until it is kept; the keeper reads it once a turn until it acts,
/// and writes the negation once the store is durable. A granule of its own:
/// neither thread's hot lines share it.
#[repr(C, align(128))]
#[derive(Debug, Default)]
pub struct AnchorSlot(AtomicI64);

// One granule: the word never shares a line with another thread's writes.
const _: () = assert!(
    core::mem::size_of::<AnchorSlot>() == 128 && core::mem::align_of::<AnchorSlot>() == 128
);

impl AnchorSlot {
    /// Nothing to keep yet.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }

    /// The arm: `usd_1e6` (> 0) is the session's anchor — keep it.
    #[inline]
    pub fn publish(&self, usd_1e6: i64) {
        debug_assert!(usd_1e6 > 0, "only a positive worth anchors");
        self.0.store(usd_1e6, Ordering::Release);
    }

    /// The arm: `usd_1e6` is stored (the keeper wrote its negation).
    #[inline]
    #[must_use]
    pub fn is_kept(&self, usd_1e6: i64) -> bool {
        self.0.load(Ordering::Acquire) == -usd_1e6
    }

    /// The keeper: the anchor waiting to be stored, if any.
    #[inline]
    #[must_use]
    pub fn pending(&self) -> Option<i64> {
        let v = self.0.load(Ordering::Acquire);
        (v > 0).then_some(v)
    }

    /// The keeper: `usd_1e6` is stored durably.
    #[inline]
    pub fn mark_kept(&self, usd_1e6: i64) {
        self.0.store(-usd_1e6, Ordering::Release);
    }
}

/// What one [`AnchorKeeper`] pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// Nothing handed over yet, or the keeper is done.
    Idle,
    /// The anchor (USD ×1e6) is stored durably, and the arm told.
    Stored(i64),
    /// The store failed (why): the gateway is halted.
    Failed(String),
}

/// **XMM XH5b-2 — slot 6's anchor keeper**, on its gateway's thread (E7:
/// the bound outlives the process). It stores the arm's fresh anchor in
/// the slot's state file once, off the engine thread; the arm reports
/// itself reconciled — so the router lets the slot quote — only once it
/// reads the anchor kept. **A store that fails halts the gateway**
/// ([`HALT_ANCHOR_UNKEPT`]): the slot never quotes on an anchor a restart
/// would lose. XMM XH5c-2: the stamp it stored the anchor under
/// ([`Self::set_unix_s`]) ties the session's top-up spend to it
/// (`crate::topup::TopupKeeper::anchored_at`).
pub struct AnchorKeeper {
    slot: Arc<AnchorSlot>,
    path: std::path::PathBuf,
    master: [u8; 20],
    stats: Arc<GatewayStats>,
    set_unix_s: u64,
    done: bool,
}

// 72 B: moved once, at boot, into the gateway's thread (noted there).
const _: () = assert!(core::mem::size_of::<AnchorKeeper>() == 72);

impl AnchorKeeper {
    /// Boot: keep what `slot` hands over in `path`, for `master`; halt
    /// through `stats` (the gateway's).
    #[must_use]
    pub fn new(
        slot: Arc<AnchorSlot>,
        path: std::path::PathBuf,
        master: [u8; 20],
        stats: Arc<GatewayStats>,
    ) -> Self {
        Self {
            slot,
            path,
            master,
            stats,
            set_unix_s: 0,
            done: false,
        }
    }

    /// One pass, once a gateway turn: one load until the arm hands an
    /// anchor over, then the store — temp file, fsync, rename: a few ms,
    /// once a session, before the slot's first quote — then nothing.
    pub fn turn(&mut self) -> Kept {
        if self.done {
            return Kept::Idle;
        }
        let Some(usd_1e6) = self.slot.pending() else {
            return Kept::Idle;
        };
        self.done = true;
        let a = PnlAnchor {
            address: self.master,
            usdc_1e6: usd_1e6,
            set_unix_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        };
        match store(&self.path, a) {
            Ok(()) => {
                self.set_unix_s = a.set_unix_s;
                self.slot.mark_kept(usd_1e6);
                Kept::Stored(usd_1e6)
            }
            Err(e) => {
                self.stats.halt(HALT_ANCHOR_UNKEPT);
                Kept::Failed(e)
            }
        }
    }

    /// Where it keeps the anchor.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// XMM XH5c-2: the stamp the anchor was stored under (its line's
    /// `set_unix_s`); `0` until it is stored.
    #[must_use]
    pub const fn set_unix_s(&self) -> u64 {
        self.set_unix_s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: [u8; 20] = [0xAB; 20];
    const OTHER: [u8; 20] = [0xCD; 20];

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mv-anchor-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The line round-trips, and only for the address it was written
    /// for.
    #[test]
    fn the_anchor_round_trips_for_its_own_address_only() {
        let a = PnlAnchor {
            address: ADDR,
            usdc_1e6: 8_630_000,
            set_unix_s: 1_789_600_000,
        };
        let line = a.to_line();
        assert_eq!(
            line,
            "0xabababababababababababababababababababab\t8630000\t1789600000\n"
        );
        assert_eq!(PnlAnchor::from_line(&line, ADDR), Some(a));
        assert_eq!(
            PnlAnchor::from_line(&line.to_uppercase(), ADDR),
            Some(a),
            "hex case-blind"
        );
        assert_eq!(
            PnlAnchor::from_line(&line, OTHER),
            None,
            "another account's anchor"
        );
    }

    /// Every malformed shape is "not anchored", never a panic and
    /// never a number.
    #[test]
    fn a_malformed_line_is_no_anchor() {
        let addr = crate::config::hex20(&ADDR);
        for bad in [
            String::new(),
            addr.clone(),
            format!("{addr}\t8630000"),
            format!("{addr}\t0\t1"),
            format!("{addr}\t-5\t1"),
            format!("{addr}\tx\t1"),
            format!("{addr}\t8630000\t1\textra"),
            format!("{addr}\t8630000\t-1"),
        ] {
            assert_eq!(PnlAnchor::from_line(&bad, ADDR), None, "{bad:?}");
        }
    }

    /// `load` on a missing file is the first boot; `store` then `load`
    /// is the restart.
    #[test]
    fn load_is_none_until_stored_then_the_stored_anchor() {
        let dir = scratch("io");
        let p = dir.join(DEFAULT_STATE_PATH);
        assert_eq!(load(&p, ADDR), None);
        let a = PnlAnchor {
            address: ADDR,
            usdc_1e6: 9_800_000,
            set_unix_s: 7,
        };
        store(&p, a).expect("stores");
        assert_eq!(load(&p, ADDR), Some(a));
        assert_eq!(load(&p, OTHER), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// XMM XH5b-2: the strict read — no file is a session's first boot,
    /// this master's line its anchor; anything else refuses (another
    /// master's session not ended, a malformed line).
    #[test]
    fn the_strict_read_refuses_what_is_not_this_master_s_anchor() {
        let dir = scratch("own");
        let p = dir.join("xmm-pnl-anchor.state");
        assert_eq!(load_own(&p, ADDR), Ok(None));
        let a = PnlAnchor {
            address: ADDR,
            usdc_1e6: 1_000_000_000,
            set_unix_s: 9,
        };
        store(&p, a).expect("stores");
        assert_eq!(load_own(&p, ADDR), Ok(Some(a)));
        let e = load_own(&p, OTHER).expect_err("another master's session");
        assert!(e.contains("moving the file aside"), "{e}");
        std::fs::write(&p, "garbage\n").expect("write");
        assert!(load_own(&p, ADDR).is_err(), "a malformed line");
        std::fs::write(&p, "").expect("write");
        assert!(load_own(&p, ADDR).is_err(), "an empty file");
        std::fs::write(&p, "a".repeat(200)).expect("write");
        let e = load_own(&p, ADDR).expect_err("an overlong file");
        assert!(e.contains("overlong"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// XMM XH5b-2: the hand-over — nothing kept until the arm publishes;
    /// then one store, and the arm reads kept only the value it published.
    #[test]
    fn the_keeper_stores_the_published_anchor_once() {
        let dir = scratch("keep");
        let p = dir.join("xmm-pnl-anchor.state");
        let (_eng, gw) = crate::gateway::hl_duplex();
        let slot = Arc::new(AnchorSlot::new());
        let mut k = AnchorKeeper::new(Arc::clone(&slot), p.clone(), ADDR, Arc::clone(&gw.stats));
        assert_eq!(k.turn(), Kept::Idle, "nothing handed over");
        slot.publish(1_000_000_000);
        assert!(!slot.is_kept(1_000_000_000), "not before the store");
        assert_eq!(k.set_unix_s(), 0, "no stamp before the store");
        assert_eq!(k.turn(), Kept::Stored(1_000_000_000));
        assert!(slot.is_kept(1_000_000_000));
        assert!(!slot.is_kept(999), "kept is the value published");
        assert_eq!(k.path(), p.as_path());
        let a = load_own(&p, ADDR).expect("read").expect("stored");
        assert_eq!(a.usdc_1e6, 1_000_000_000);
        assert!(
            k.set_unix_s() > 0 && k.set_unix_s() == a.set_unix_s,
            "XH5c-2: the stamp stored"
        );
        assert_eq!(k.turn(), Kept::Idle, "once a session");
        assert_eq!(gw.stats.halt_reason(), exec_gateway::HALT_NONE);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// XMM XH5b-2: a store that fails halts the gateway, and the anchor is
    /// never read kept.
    #[test]
    fn a_failed_store_halts_the_gateway_and_keeps_nothing() {
        let dir = scratch("fail");
        let p = dir.join("no-such-dir").join("xmm-pnl-anchor.state");
        let (_eng, gw) = crate::gateway::hl_duplex();
        let slot = Arc::new(AnchorSlot::new());
        let mut k = AnchorKeeper::new(Arc::clone(&slot), p, ADDR, Arc::clone(&gw.stats));
        slot.publish(5_000_000);
        assert!(matches!(k.turn(), Kept::Failed(_)));
        assert_eq!(gw.stats.halt_reason(), HALT_ANCHOR_UNKEPT);
        assert!(!slot.is_kept(5_000_000));
        assert_eq!(slot.pending(), Some(5_000_000), "never marked kept");
        assert_eq!(k.set_unix_s(), 0, "no stamp: nothing stored");
        assert_eq!(k.turn(), Kept::Idle, "one try: the gateway is halted");
        std::fs::remove_dir_all(&dir).ok();
    }
}
