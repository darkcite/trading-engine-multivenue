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
//! BOOT/OFFLINE DOCTRINE: [`load`] runs once at boot; [`store`] runs
//! once per session, from the reconciler's idle-path cycle. Allocation
//! is fine on both, and neither is on the tick path.

/// Default location of the anchor file — beside the budget's.
pub const DEFAULT_STATE_PATH: &str = "exec-pnl-anchor.state";

/// The anchor: whose, how much, and when it was set.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PnlAnchor {
    /// The MASTER account the balance belongs to.
    pub address: [u8; 20],
    /// Spot USDC at the first flat reconciliation, ×1e6. Always > 0:
    /// an account with no USDC has nothing to bound, and `0` is the
    /// in-memory spelling of "not anchored yet".
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
        Some(Self { address, usdc_1e6, set_unix_s })
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
/// Called once per session from the reconciler's idle cycle, never the
/// tick path.
///
/// # Errors
/// The temp write, the fsync or the rename failed; the message names
/// the path.
pub fn store(path: &std::path::Path, a: PnlAnchor) -> Result<(), String> {
    core_io::write_atomic(path, &a.to_line())
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
        let a = PnlAnchor { address: ADDR, usdc_1e6: 8_630_000, set_unix_s: 1_789_600_000 };
        let line = a.to_line();
        assert_eq!(
            line,
            "0xabababababababababababababababababababab\t8630000\t1789600000\n"
        );
        assert_eq!(PnlAnchor::from_line(&line, ADDR), Some(a));
        assert_eq!(PnlAnchor::from_line(&line.to_uppercase(), ADDR), Some(a), "hex case-blind");
        assert_eq!(PnlAnchor::from_line(&line, OTHER), None, "another account's anchor");
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
        let a = PnlAnchor { address: ADDR, usdc_1e6: 9_800_000, set_unix_s: 7 };
        store(&p, a).expect("stores");
        assert_eq!(load(&p, ADDR), Some(a));
        assert_eq!(load(&p, OTHER), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
