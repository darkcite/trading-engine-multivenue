// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # held — the legs the wallet holds, across a restart (`pmj-held.state`)
//!
//! jup-gw books each fill's contracts and cost into its held table, keyed
//! by the Jupiter position, and hands the owner the rendered table
//! whenever it changes (`JupGateway::take_held`); the owner writes it
//! beside `exec.toml`, and the boot restores it to the gateway (its
//! positions and claims) and to the arm (its worth: USDC plus each
//! unclaimed leg at cost). Without it a restart would drop a held leg from
//! the worth — a false loss against the session anchor the boot restores.
//!
//! COPY-DOCTRINE: cold — rendered on a change (a fill, a claim), parsed
//! once at boot; never on the order path.
//!
//! The file is text, one leg a line:
//!
//! ```text
//! pmj-held v1 owner=<base58>
//! leg instance=<u32> outcome=0|1 state=1|2 contracts_micro=<u64> cost_micro=<u64> position=<base58>
//! ```

use core_crypto::{base58_decode, base58_encode};

use crate::render::B58_KEY_MAX;
use crate::tx::KEY_LEN;

/// Legs held at once.
pub const HELD_MAX: usize = 32;
/// The file's bound.
pub const HELD_FILE_MAX: usize = 96 + HELD_MAX * 192;

/// Bought, not claimed.
pub const HELD_OPEN: u8 = 1;
/// Its claim was built, guarded and signed (the route is J0's).
pub const HELD_CLAIM_READY: u8 = 2;

/// One held leg.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HeldLeg {
    /// The Jupiter position (zero: unknown).
    pub position: [u8; KEY_LEN],
    /// Contracts, micro units.
    pub contracts_micro: u64,
    /// Cost, micro USD.
    pub cost_micro: u64,
    /// Its window's instance id.
    pub instance: u32,
    /// 0 Up, 1 Down.
    pub outcome: u8,
    /// 0 = free, [`HELD_OPEN`], [`HELD_CLAIM_READY`].
    pub state: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 2],
}

impl HeldLeg {
    /// The empty leg.
    pub const EMPTY: Self = Self {
        position: [0; KEY_LEN],
        contracts_micro: 0,
        cost_micro: 0,
        instance: 0,
        outcome: 0,
        state: 0,
        _pad: [0; 2],
    };
}

fn put(dst: &mut [u8], at: usize, b: &[u8]) -> usize {
    let end = (at + b.len()).min(dst.len());
    dst[at..end].copy_from_slice(&b[..end - at]);
    end
}

fn put_u64(dst: &mut [u8], at: usize, v: u64) -> usize {
    let mut d = [0u8; 20];
    let mut x = v;
    let mut k = 0usize;
    loop {
        d[19 - k] = b'0' + (x % 10) as u8;
        k += 1;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    put(dst, at, &d[20 - k..])
}

/// The held file of the non-free `legs` for `owner_b58` into `dst`
/// (≥ [`HELD_FILE_MAX`]); its length.
#[must_use]
pub fn render_held(dst: &mut [u8], owner_b58: &[u8], legs: &[HeldLeg]) -> usize {
    let mut at = put(dst, 0, b"pmj-held v1 owner=");
    at = put(dst, at, owner_b58);
    at = put(dst, at, b"\n");
    let mut i = 0usize;
    while i < legs.len() && i < HELD_MAX {
        let l = &legs[i];
        i += 1;
        if l.state == 0 {
            continue;
        }
        at = put(dst, at, b"leg instance=");
        at = put_u64(dst, at, u64::from(l.instance));
        at = put(dst, at, b" outcome=");
        at = put_u64(dst, at, u64::from(l.outcome & 1));
        at = put(dst, at, b" state=");
        at = put_u64(dst, at, u64::from(l.state));
        at = put(dst, at, b" contracts_micro=");
        at = put_u64(dst, at, l.contracts_micro);
        at = put(dst, at, b" cost_micro=");
        at = put_u64(dst, at, l.cost_micro);
        at = put(dst, at, b" position=");
        let mut k = [0u8; B58_KEY_MAX];
        let n = base58_encode(&l.position, &mut k).unwrap_or(0);
        at = put(dst, at, &k[..n]);
        at = put(dst, at, b"\n");
    }
    at
}

/// Why a held file was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HeldErr {
    /// Not a `pmj-held v1` file.
    Header,
    /// The file is another wallet's.
    Owner,
    /// A line that does not read (its 1-based number).
    Line(usize),
    /// More legs than [`HELD_MAX`].
    Full,
}

fn num(v: &[u8]) -> Option<u64> {
    if v.is_empty() || v.len() > 19 {
        return None;
    }
    let mut n = 0u64;
    for &d in v {
        if !d.is_ascii_digit() {
            return None;
        }
        n = n * 10 + u64::from(d - b'0');
    }
    Some(n)
}

/// Parse a held file for `owner_b58` into `out`; how many legs it holds.
/// Fail closed: a file that does not read whole is refused whole (the
/// boot says so and the operator decides — never a silent empty ledger).
///
/// # Errors
/// [`HeldErr`].
pub fn parse_held(text: &[u8], owner_b58: &[u8], out: &mut [HeldLeg; HELD_MAX]) -> Result<usize, HeldErr> {
    let mut lines = text.split(|&b| b == b'\n');
    let head = lines.next().ok_or(HeldErr::Header)?;
    let Some(owner) = head.strip_prefix(b"pmj-held v1 owner=") else {
        return Err(HeldErr::Header);
    };
    if owner != owner_b58 {
        return Err(HeldErr::Owner);
    }
    let mut n = 0usize;
    for (ln, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let bad = HeldErr::Line(ln + 2);
        let Some(rest) = line.strip_prefix(b"leg ") else {
            return Err(bad);
        };
        let mut l = HeldLeg::EMPTY;
        let mut seen = 0u8;
        for field in rest.split(|&b| b == b' ') {
            let Some(eq) = field.iter().position(|&b| b == b'=') else {
                return Err(bad);
            };
            let (k, v) = (&field[..eq], &field[eq + 1..]);
            match k {
                b"instance" => {
                    l.instance = u32::try_from(num(v).ok_or(bad)?).map_err(|_| bad)?;
                    seen |= 1;
                }
                b"outcome" => {
                    l.outcome = match v {
                        b"0" => 0,
                        b"1" => 1,
                        _ => return Err(bad),
                    };
                    seen |= 2;
                }
                b"state" => {
                    l.state = match v {
                        b"1" => HELD_OPEN,
                        b"2" => HELD_CLAIM_READY,
                        _ => return Err(bad),
                    };
                    seen |= 4;
                }
                b"contracts_micro" => {
                    l.contracts_micro = num(v).ok_or(bad)?;
                    seen |= 8;
                }
                b"cost_micro" => {
                    l.cost_micro = num(v).ok_or(bad)?;
                    seen |= 16;
                }
                b"position" => {
                    match base58_decode(v, &mut l.position) {
                        Ok(KEY_LEN) => {}
                        _ => return Err(bad),
                    }
                    seen |= 32;
                }
                _ => return Err(bad),
            }
        }
        if seen != 63 {
            return Err(bad);
        }
        if n >= HELD_MAX {
            return Err(HeldErr::Full);
        }
        out[n] = l;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &[u8] = b"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z";

    #[test]
    fn the_held_file_round_trips_and_skips_free_legs() {
        let mut legs = [HeldLeg::EMPTY; HELD_MAX];
        legs[0] = HeldLeg {
            position: [7; KEY_LEN],
            contracts_micro: 5_263_157,
            cost_micro: 5_000_000,
            instance: 0x8000_0000 | 42,
            outcome: 1,
            state: HELD_OPEN,
            _pad: [0; 2],
        };
        legs[3] = HeldLeg {
            state: HELD_CLAIM_READY,
            instance: 9,
            ..legs[0]
        };
        let mut buf = vec![0u8; HELD_FILE_MAX];
        let n = render_held(&mut buf, OWNER, &legs);
        let text = &buf[..n];
        assert!(text.starts_with(b"pmj-held v1 owner=FVen3X"));
        assert_eq!(text.iter().filter(|&&b| b == b'\n').count(), 3, "a header and two legs");
        let mut back = [HeldLeg::EMPTY; HELD_MAX];
        assert_eq!(parse_held(text, OWNER, &mut back), Ok(2));
        assert_eq!(back[0], legs[0]);
        assert_eq!(back[1], legs[3]);
        // A full table fits the bound.
        let full = [legs[0]; HELD_MAX];
        assert!(render_held(&mut buf, OWNER, &full) <= HELD_FILE_MAX);
    }

    #[test]
    fn a_held_file_that_does_not_read_whole_is_refused_whole() {
        let mut out = [HeldLeg::EMPTY; HELD_MAX];
        assert_eq!(parse_held(b"pm15-held v1 owner=x\n", OWNER, &mut out), Err(HeldErr::Header));
        let other = b"pmj-held v1 owner=11111111111111111111111111111111\n";
        assert_eq!(parse_held(other, OWNER, &mut out), Err(HeldErr::Owner));
        let mut good = b"pmj-held v1 owner=FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z\n".to_vec();
        good.extend_from_slice(b"leg instance=1 outcome=0 state=1 contracts_micro=1 cost_micro=1 position=11111111111111111111111111111111\n");
        assert_eq!(parse_held(&good, OWNER, &mut out), Ok(1));
        let missing = String::from_utf8(good.clone()).unwrap().replace(" cost_micro=1", "");
        assert_eq!(parse_held(missing.as_bytes(), OWNER, &mut out), Err(HeldErr::Line(2)));
        let bad_outcome = String::from_utf8(good.clone()).unwrap().replace("outcome=0", "outcome=2");
        assert_eq!(parse_held(bad_outcome.as_bytes(), OWNER, &mut out), Err(HeldErr::Line(2)));
        let bad_key = String::from_utf8(good).unwrap().replace("position=1111", "position=0OIl");
        assert_eq!(parse_held(bad_key.as_bytes(), OWNER, &mut out), Err(HeldErr::Line(2)));
    }
}
