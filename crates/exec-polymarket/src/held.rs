// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The legs the account holds, as pm-gw keeps them and the state file
//! carries them across a restart (PM15-6f).
//!
//! The arm values the account as pUSD plus each unredeemed leg at its
//! cost, and asks a won leg redeemed. Before this, a restart emptied that
//! ledger: a held leg vanished from the worth — a false loss against the
//! session anchor the boot restores — and a won leg was never redeemed.
//! Now pm-gw mirrors every leg (the arm's `CMD_WATCH` on each fill, its
//! `CMD_REDEEM` at settlement, `WATCH_STOP` when the leg leaves) with what
//! the reads and the redemption need after its window has left the token
//! table — the token's decimal id and the market's condition — and writes
//! the table to `pm15-held.state` beside `exec.toml` whenever it changes
//! (pm-gw's idle path, never in a window's quiet tail). The boot restores
//! the file to both: the arm's ledger, pm-gw's reads and redemption.
//!
//! **A leg whose roll the restarted engine will never see settle** (its
//! window ended before the boot, the ingress tracking only the current
//! and next ones) is expired by pm-gw [`HELD_EXPIRE_S`] after its window's
//! end: asked redeemed (the adapter pays what the chain says) and told to
//! the arm (`EVT_EXPIRED`), which books it as settled unknown — at 0, the
//! conservative book — so its later zero balance reads as redeemed, never
//! as a drift.
//!
//! The file is text, one leg a line:
//!
//! ```text
//! pm15-held v1 owner=0x…
//! leg instance=… leg=0|1 state=1|2|3 shares_1e6=… cost_1e6=… condition=0x<64 hex> token=<decimal>
//! ```

use crate::num::{hex32, put, put_hex, put_u64};

/// Legs held at once — the arm's ledger size.
pub const HELD_MAX: usize = 32;
/// A held leg whose window ended this long ago without a settlement is
/// expired, s — after the ingress retires an unresolved window (10 min),
/// so a settlement the engine does see always comes first.
pub const HELD_EXPIRE_S: u64 = 900;
/// The state file's size bound.
pub const HELD_FILE_MAX: usize = 64 + HELD_MAX * 288;

/// Bought, its roll not settled.
pub const HELD_OPEN: u8 = 1;
/// Settled won, redemption asked.
pub const HELD_WON: u8 = 2;
/// Settled unknown (or expired), redemption asked.
pub const HELD_UNKNOWN: u8 = 3;

/// One held leg.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HeldLeg {
    /// Shares ×1e6.
    pub shares_1e6: i64,
    /// Cost ×1e6.
    pub cost_1e6: i64,
    /// Its window's instance id.
    pub instance: u32,
    /// 0 Up, 1 Down.
    pub leg: u8,
    /// [`HELD_OPEN`], [`HELD_WON`] or [`HELD_UNKNOWN`].
    pub state: u8,
    /// Bytes of `dec` used (0: unknown — its window had left the table).
    pub dec_len: u8,
    /// Explicit padding — always zero.
    pub _pad: u8,
    /// The market's condition id (zero: unknown).
    pub condition: [u8; 32],
    /// The token's decimal id (the account read's `token_id`).
    pub dec: [u8; 80],
}

impl HeldLeg {
    /// The empty leg.
    pub const EMPTY: Self = Self {
        shares_1e6: 0,
        cost_1e6: 0,
        instance: 0,
        leg: 0,
        state: 0,
        dec_len: 0,
        _pad: 0,
        condition: [0; 32],
        dec: [0; 80],
    };

    /// The token's decimal id.
    #[inline]
    #[must_use]
    pub fn dec(&self) -> &[u8] {
        &self.dec[..(self.dec_len as usize).min(80)]
    }
}

/// The state file of `legs` for `owner` (EIP-55) into `dst`; its length.
///
/// # Panics
/// `dst` under [`HELD_FILE_MAX`] or more than [`HELD_MAX`] legs (a boot
/// sizing bug).
pub fn render_held(dst: &mut [u8], owner: &[u8; 42], legs: &[HeldLeg]) -> usize {
    assert!(
        dst.len() >= HELD_FILE_MAX && legs.len() <= HELD_MAX,
        "the held file's bound"
    );
    let mut at = put(dst, 0, b"pm15-held v1 owner=");
    at = put(dst, at, owner);
    at = put(dst, at, b"\n");
    let mut i = 0usize;
    while i < legs.len() {
        let l = &legs[i];
        at = put(dst, at, b"leg instance=");
        at = put_u64(dst, at, u64::from(l.instance));
        at = put(dst, at, b" leg=");
        at = put_u64(dst, at, u64::from(l.leg & 1));
        at = put(dst, at, b" state=");
        at = put_u64(dst, at, u64::from(l.state));
        at = put(dst, at, b" shares_1e6=");
        at = put_u64(dst, at, l.shares_1e6.max(0) as u64);
        at = put(dst, at, b" cost_1e6=");
        at = put_u64(dst, at, l.cost_1e6.max(0) as u64);
        at = put(dst, at, b" condition=0x");
        at = put_hex(dst, at, &l.condition);
        at = put(dst, at, b" token=");
        at = put(dst, at, l.dec());
        at = put(dst, at, b"\n");
        i += 1;
    }
    at
}

/// The legs of a state file — boot-only (it allocates).
///
/// # Errors
/// Another account's file, a line that does not read, or more than
/// [`HELD_MAX`] legs.
pub fn parse_held(text: &str, owner: &[u8; 42]) -> Result<Vec<HeldLeg>, String> {
    let mut lines = text.lines();
    let head = lines.next().unwrap_or_default();
    let Some(who) = head.strip_prefix("pm15-held v1 owner=") else {
        return Err(String::from("not a pm15-held v1 file"));
    };
    if !who.trim().as_bytes().eq_ignore_ascii_case(owner) {
        return Err(String::from("another account's held legs"));
    }
    let mut out = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix("leg ") else {
            return Err(format!("a line that is not a leg: {line:?}"));
        };
        let mut l = HeldLeg::EMPTY;
        let mut seen = 0u8;
        for kv in rest.split_whitespace() {
            let Some((k, v)) = kv.split_once('=') else {
                return Err(format!("a field without a value: {kv:?}"));
            };
            let num = || v.parse::<u64>().map_err(|_| format!("{k} is not a number"));
            match k {
                "instance" => {
                    l.instance = u32::try_from(num()?).map_err(|_| String::from("instance"))?;
                    seen |= 1;
                }
                "leg" => {
                    l.leg = u8::from(num()? != 0);
                    seen |= 2;
                }
                "state" => {
                    let s = num()?;
                    if !(1..=3).contains(&s) {
                        return Err(format!("state {s}"));
                    }
                    l.state = s as u8;
                    seen |= 4;
                }
                "shares_1e6" => {
                    l.shares_1e6 = i64::try_from(num()?).map_err(|_| String::from("shares"))?;
                    seen |= 8;
                }
                "cost_1e6" => {
                    l.cost_1e6 = i64::try_from(num()?).map_err(|_| String::from("cost"))?;
                    seen |= 16;
                }
                "condition" => {
                    l.condition = hex32(v.as_bytes()).ok_or_else(|| String::from("condition"))?;
                    seen |= 32;
                }
                "token" => {
                    if v.len() > 80 || !v.bytes().all(|c| c.is_ascii_digit()) {
                        return Err(String::from("token"));
                    }
                    l.dec_len = v.len() as u8;
                    // COPY: boot, once a leg — the token's digits into the
                    // fixed record.
                    l.dec[..v.len()].copy_from_slice(v.as_bytes());
                    seen |= 64;
                }
                _ => return Err(format!("an unknown field: {k}")),
            }
        }
        if seen != 127 {
            return Err(format!("a leg without every field: {line:?}"));
        }
        if out.len() == HELD_MAX {
            return Err(format!("more than {HELD_MAX} legs"));
        }
        out.push(l);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &[u8; 42] = b"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    fn leg(instance: u32, state: u8) -> HeldLeg {
        let mut l = HeldLeg::EMPTY;
        l.instance = instance;
        l.leg = 1;
        l.state = state;
        l.shares_1e6 = 268_810_000;
        l.cost_1e6 = 249_993_300;
        l.condition = [0x53; 32];
        l.dec_len = 4;
        l.dec[..4].copy_from_slice(b"1234");
        l
    }

    #[test]
    fn the_file_round_trips_and_names_its_account() {
        let legs = [leg(7, HELD_OPEN), leg(9, HELD_WON)];
        let mut buf = vec![0u8; HELD_FILE_MAX];
        let n = render_held(&mut buf, OWNER, &legs);
        let text = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(text.starts_with("pm15-held v1 owner=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\nleg instance=7 leg=1 state=1 shares_1e6=268810000 cost_1e6=249993300 condition=0x5353"), "{text}");
        assert_eq!(parse_held(text, OWNER).unwrap(), legs.to_vec());
        let lower = *b"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
        assert_eq!(
            parse_held(text, &lower).unwrap().len(),
            2,
            "the address's case is free"
        );
        let other = *b"0x0000000000000000000000000000000000000001";
        assert!(parse_held(text, &other)
            .unwrap_err()
            .contains("another account"));
        assert_eq!(
            parse_held(
                "pm15-held v1 owner=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\n",
                OWNER
            )
            .unwrap(),
            vec![]
        );
    }

    #[test]
    fn a_file_that_does_not_read_is_refused() {
        let head = "pm15-held v1 owner=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\n";
        for bad in [
            "garbage",
            "leg instance=7",
            "leg instance=7 leg=1 state=4 shares_1e6=1 cost_1e6=1 condition=0x00 token=1",
            "leg instance=7 leg=1 state=1 shares_1e6=1 cost_1e6=1 condition=0x00 token=12a",
            "leg instance=7 leg=1 state=1 shares_1e6=1 cost_1e6=1 condition=0x0000000000000000000000000000000000000000000000000000000000000000 token=1 extra=2",
        ] {
            assert!(parse_held(&format!("{head}{bad}\n"), OWNER).is_err(), "{bad}");
        }
        assert!(parse_held("pm15-anchor v1 owner=x", OWNER).is_err());
    }
}
