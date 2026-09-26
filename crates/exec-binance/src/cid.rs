// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The client order id — LAW E-9 carried (plan §3.7, §13.4 D7).**
//!
//! ```text
//! mv  EEEEEEEE  S  OOOOOOOOOOOOOOOO  00000     32 chars, [a-z0-9] only
//! │   │         │  │                 └ reserved tail, always "00000" (D7)
//! │   │         │  └ client_oid, 16 lower-hex (u64)
//! │   │         └ slot, 1 lower-hex (0..=7)
//! │   └ boot epoch = low 32 bits of unix seconds at boot, 8 lower-hex
//! └ prefix
//! ```
//!
//! One width on every product (D7): Binance Stocks needs ≥ 32 characters
//! (`^[a-zA-Z0-9-_]{32,36}$`), and 32 still fits the spot pattern
//! `^[a-zA-Z0-9-_]{1,36}$` and the UM/CM pattern
//! `^[\.A-Z\:/a-z0-9_-]{1,36}$`. One width is one codec with fixed offsets.
//!
//! **Decoding** ([`classify`]) reads the fixed offsets and parses hex with
//! SWAR — one validation and one pack per 8 digits, no per-digit branch.
//! Every id the venue reports on an instrument is one of:
//!
//! * [`CidClass::Ours`] — this boot's epoch, a slot we run, the zero tail;
//! * [`CidClass::Orphan`] — a well-formed `mv` id from another epoch: swept
//!   before the first `reconciled` (BX-9);
//! * [`CidClass::Liquidation`] / [`CidClass::Adl`] / [`CidClass::Settlement`]
//!   — venue-originated (`autoclose-`, `adl_autoclose`,
//!   `settlement_autoclose-`): booked to the instrument's OWNER slot (BX-7),
//!   the last with `FILL_FLAG_SETTLEMENT`;
//! * [`CidClass::Foreign`] — anything else, including an `mv` id with a
//!   non-zero tail, a slot past 7, upper-case hex or the wrong length
//!   (BX-8: recon drift on a dedicated account, a drift HALT on a shared
//!   one).

/// The id's length, every product (D7).
pub const CID_LEN: usize = 32;
/// Our prefix.
pub const CID_PREFIX: [u8; 2] = *b"mv";
/// The reserved tail (D7).
pub const CID_TAIL: [u8; 5] = *b"00000";
/// The highest slot an id of ours can name (`EXEC_SLOTS - 1`).
pub const CID_SLOT_MAX: u8 = 7;

const OFF_EPOCH: usize = 2;
const OFF_SLOT: usize = 10;
const OFF_OID: usize = 11;
const OFF_TAIL: usize = 27;

const _: () = assert!(OFF_TAIL + CID_TAIL.len() == CID_LEN);
const _: () = assert!(OFF_OID + 16 == OFF_TAIL);

/// Liquidation (`autoclose-…`).
const PFX_LIQUIDATION: &[u8] = b"autoclose-";
/// Auto-deleverage (`adl_autoclose…`).
const PFX_ADL: &[u8] = b"adl_autoclose";
/// Delivery, delisting close, and (for options, from the exercise record)
/// settlement (`settlement_autoclose-…`).
const PFX_SETTLEMENT: &[u8] = b"settlement_autoclose-";

/// What an id the venue reported is (module docs).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CidClass {
    /// This boot's order: the slot and the member's `client_oid` it was
    /// placed under.
    Ours {
        /// The strategy slot.
        slot: u8,
        /// The member's id at placement.
        client_oid: u64,
    },
    /// A well-formed id of ours from another boot epoch.
    Orphan {
        /// That boot's epoch.
        epoch: u32,
        /// The slot it named.
        slot: u8,
        /// The member id it carried.
        client_oid: u64,
    },
    /// The venue liquidated (`autoclose-`).
    Liquidation,
    /// The venue auto-deleveraged (`adl_autoclose`).
    Adl,
    /// A delivery / delisting / exercise settlement (`settlement_autoclose-`).
    Settlement,
    /// Not ours and not the venue's own.
    Foreign,
}

/// The boot's rendered prefix: `mv` + the epoch's 8 hex digits. Built
/// once at boot; [`CidPrefix::render`] writes the rest per order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CidPrefix {
    bytes: [u8; OFF_SLOT],
    epoch: u32,
}

impl CidPrefix {
    /// The prefix of the boot whose epoch is `epoch` (low 32 bits of the
    /// unix seconds at boot).
    #[must_use]
    pub const fn new(epoch: u32) -> Self {
        let mut bytes = [0u8; OFF_SLOT];
        bytes[0] = CID_PREFIX[0];
        bytes[1] = CID_PREFIX[1];
        let h = hex8(epoch);
        let mut i = 0;
        while i < 8 {
            bytes[OFF_EPOCH + i] = h[i];
            i += 1;
        }
        Self { bytes, epoch }
    }

    /// The epoch this prefix renders.
    #[inline(always)]
    #[must_use]
    pub const fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Write the full id of (`slot`, `client_oid`) into `out` — on the
    /// order path, `out` is the id's place in the request frame itself
    /// ([`crate::wsapi::Part::Cid`]).
    ///
    /// `slot` must be ≤ [`CID_SLOT_MAX`] (the arm refuses any other slot
    /// before a command exists).
    #[inline(always)]
    pub fn render(&self, slot: u8, client_oid: u64, out: &mut [u8; CID_LEN]) {
        debug_assert!(slot <= CID_SLOT_MAX, "a slot past EXEC_SLOTS");
        let hi = hex8((client_oid >> 32) as u32);
        let lo = hex8(client_oid as u32);
        // COPY: the 10 B prefix, 2 × 8 B of hex and the 5 B tail into the
        // id's place in the frame — this IS the render (32 B per request);
        // the prefix is pre-rendered at boot so only the digits are new —
        // rendering the prefix per request was rejected (more work).
        out[..OFF_SLOT].copy_from_slice(&self.bytes);
        out[OFF_SLOT] = HEX[(slot & 0x0F) as usize];
        out[OFF_OID..OFF_OID + 8].copy_from_slice(&hi);
        out[OFF_OID + 8..OFF_TAIL].copy_from_slice(&lo);
        out[OFF_TAIL..].copy_from_slice(&CID_TAIL);
    }

    /// Classify an id the venue reported (module docs). Never panics, for
    /// any bytes.
    #[must_use]
    pub fn classify(&self, id: &[u8]) -> CidClass {
        classify(id, self.epoch)
    }
}

const HEX: [u8; 16] = *b"0123456789abcdef";

/// `v` as 8 lower-hex digits, most significant first (SWAR: spread the
/// nibbles into bytes, then one add per byte; no table, no branch).
#[inline(always)]
#[must_use]
const fn hex8(v: u32) -> [u8; 8] {
    let mut x = v as u64;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    // Byte k now holds nibble k (least significant first). A nibble ≥ 10
    // carries into bit 4 when 6 is added: that bit picks the letter offset.
    let letters = ((x + 0x0606_0606_0606_0606) >> 4) & 0x0101_0101_0101_0101;
    let ascii = x + 0x3030_3030_3030_3030 + letters * (b'a' - b'0' - 10) as u64;
    ascii.to_be_bytes()
}

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = 0x8080_8080_8080_8080;

/// The high bit of each byte of `v` set iff that byte is ≥ `k`, for bytes
/// < 0x80 and `k` ≤ 0x80 (no borrow crosses a byte).
#[inline(always)]
const fn ge(v: u64, k: u8) -> u64 {
    ((v | HIGH) - (k as u64) * ONES) & HIGH
}

/// Eight bytes at `off`, most significant first: one register load.
#[inline(always)]
const fn load8(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// 8 lower-hex digits (loaded by [`load8`]) → their value; `None` for any
/// other byte (upper case included). SWAR: validate all 8 at once, then
/// pack.
#[inline(always)]
#[must_use]
const fn unhex8(v: u64) -> Option<u32> {
    if v & HIGH != 0 {
        return None;
    }
    let digit = ge(v, b'0') & !ge(v, b'9' + 1);
    let lower = ge(v, b'a') & !ge(v, b'f' + 1);
    if (digit | lower) != HIGH {
        return None;
    }
    // Byte k (least significant first) holds digit 7-k's nibble.
    let n = (v & 0x0F0F_0F0F_0F0F_0F0F) + (lower >> 7) * 9;
    let x = (n | (n >> 4)) & 0x00FF_00FF_00FF_00FF;
    let x = (x | (x >> 8)) & 0x0000_FFFF_0000_FFFF;
    let x = (x | (x >> 16)) & 0x0000_0000_FFFF_FFFF;
    Some(x as u32)
}

/// One hex digit (lower case) → its value.
#[inline(always)]
const fn unhex1(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Classify `id` against the boot epoch `epoch` (module docs). Never
/// panics, for any bytes of any length.
#[must_use]
pub fn classify(id: &[u8], epoch: u32) -> CidClass {
    if id.len() == CID_LEN && id[0] == CID_PREFIX[0] && id[1] == CID_PREFIX[1] {
        return classify_mv(id, epoch);
    }
    // The venue's own ids. The settlement prefix is tested before the
    // liquidation one: it CONTAINS "autoclose-" after its first word.
    if id.starts_with(PFX_SETTLEMENT) {
        CidClass::Settlement
    } else if id.starts_with(PFX_ADL) {
        CidClass::Adl
    } else if id.starts_with(PFX_LIQUIDATION) {
        CidClass::Liquidation
    } else {
        CidClass::Foreign
    }
}

#[inline(always)]
fn classify_mv(id: &[u8], epoch: u32) -> CidClass {
    debug_assert!(id.len() == CID_LEN);
    if id[OFF_TAIL..] != CID_TAIL {
        return CidClass::Foreign;
    }
    let (Some(e), Some(slot), Some(hi), Some(lo)) = (
        unhex8(load8(id, OFF_EPOCH)),
        unhex1(id[OFF_SLOT]),
        unhex8(load8(id, OFF_OID)),
        unhex8(load8(id, OFF_OID + 8)),
    ) else {
        return CidClass::Foreign;
    };
    if slot > CID_SLOT_MAX {
        return CidClass::Foreign;
    }
    let client_oid = ((hi as u64) << 32) | lo as u64;
    if e == epoch {
        CidClass::Ours { slot, client_oid }
    } else {
        CidClass::Orphan {
            epoch: e,
            slot,
            client_oid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn render(epoch: u32, slot: u8, oid: u64) -> [u8; CID_LEN] {
        let mut out = [0u8; CID_LEN];
        CidPrefix::new(epoch).render(slot, oid, &mut out);
        out
    }

    #[test]
    fn the_layout_is_the_plans() {
        let id = render(0x6512_ab0f, 3, 0x0123_4567_89ab_cdef);
        assert_eq!(&id, b"mv6512ab0f30123456789abcdef00000");
        assert!(id.iter().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()));
    }

    #[test]
    fn hex8_matches_the_formatter() {
        for v in [0u32, 1, 9, 10, 15, 16, 0xdead_beef, u32::MAX, 0x8000_0000] {
            let want = std::format!("{v:08x}");
            assert_eq!(&hex8(v), want.as_bytes(), "{v:#x}");
        }
    }

    #[test]
    fn ours_orphans_and_the_venues_own() {
        let p = CidPrefix::new(7);
        let id = render(7, 2, 42);
        assert_eq!(p.classify(&id), CidClass::Ours { slot: 2, client_oid: 42 });
        let old = render(6, 2, 42);
        assert_eq!(
            p.classify(&old),
            CidClass::Orphan { epoch: 6, slot: 2, client_oid: 42 }
        );
        assert_eq!(p.classify(b"autoclose-1695000000123"), CidClass::Liquidation);
        assert_eq!(p.classify(b"adl_autoclose"), CidClass::Adl);
        assert_eq!(
            p.classify(b"settlement_autoclose-1695000000"),
            CidClass::Settlement
        );
        assert_eq!(p.classify(b"web_AbCdEf"), CidClass::Foreign);
        assert_eq!(p.classify(b""), CidClass::Foreign);
    }

    #[test]
    fn a_malformed_mv_id_is_foreign() {
        let p = CidPrefix::new(7);
        let good = render(7, 2, 42);
        // A non-zero tail (D7 reserves it).
        let mut t = good;
        t[CID_LEN - 1] = b'1';
        assert_eq!(p.classify(&t), CidClass::Foreign);
        // Upper-case hex.
        let mut u = render(7, 2, 0xab);
        let at = u.iter().rposition(|&b| b == b'a').unwrap();
        u[at] = b'A';
        assert_eq!(p.classify(&u), CidClass::Foreign);
        // A slot past 7.
        let mut s = good;
        s[OFF_SLOT] = b'8';
        assert_eq!(p.classify(&s), CidClass::Foreign);
        // Too short / too long.
        assert_eq!(p.classify(&good[..31]), CidClass::Foreign);
        let mut long = [b'0'; 33];
        long[..32].copy_from_slice(&good);
        assert_eq!(p.classify(&long), CidClass::Foreign);
    }

    proptest! {
        #[test]
        fn render_classify_round_trips(epoch in any::<u32>(), slot in 0u8..=CID_SLOT_MAX,
                                       oid in any::<u64>(), other in any::<u32>()) {
            let id = render(epoch, slot, oid);
            prop_assert_eq!(classify(&id, epoch), CidClass::Ours { slot, client_oid: oid });
            if other != epoch {
                prop_assert_eq!(classify(&id, other),
                                CidClass::Orphan { epoch, slot, client_oid: oid });
            }
        }

        #[test]
        fn unhex8_is_the_inverse_of_hex8(v in any::<u32>()) {
            prop_assert_eq!(unhex8(u64::from_be_bytes(hex8(v))), Some(v));
        }

        #[test]
        fn unhex8_accepts_exactly_lower_hex(d in any::<[u8; 8]>()) {
            let want = if d.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                u32::from_str_radix(core::str::from_utf8(&d).unwrap(), 16).ok()
            } else {
                None
            };
            prop_assert_eq!(unhex8(load8(&d, 0)), want);
        }

        #[test]
        fn classify_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64),
                                 epoch in any::<u32>()) {
            let c = classify(&bytes, epoch);
            if let CidClass::Ours { slot, client_oid } = c {
                prop_assert_eq!(&bytes[..], &render(epoch, slot, client_oid)[..]);
            }
        }
    }
}
