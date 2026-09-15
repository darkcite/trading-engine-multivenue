// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **LAW E-9 — the cloid encodes the slot.**
//!
//! The venue echoes a client id and nothing else we chose. The
//! engine's fill fan-out routes `on_fill` by `strategy_id`. So the
//! slot has to travel inside the one field that comes back, or a fill
//! arrives with no owner and the engine has to guess — and a guessed
//! owner books a real fill against the wrong member's P&L.
//!
//! ```text
//!   byte  0    1    2      3 4 5 6 7        8 .. 15
//!        0x4D 0x56  slot   0 0 0 0 0        client_oid, big-endian
//!         'M'  'V'
//! ```
//!
//! Equivalently: high 64 bits = `0x4D56 << 48 | strategy_id << 40`,
//! low 64 bits = the member's `client_oid`.
//!
//! Three things this buys, in the order they matter:
//!
//! 1. **Recovery is a shift and a mask**, never a table lookup. The
//!    fill path does no allocation, no search and no locking to answer
//!    "whose fill is this".
//! 2. **Cross-slot uniqueness** the moment a second slot arms. Two
//!    members may both emit `client_oid = 1`; they cannot both emit
//!    the same cloid.
//! 3. **Engine-origin orders are recognisable on the venue's own
//!    books.** During an incident, an operator looking at the
//!    Hyperliquid UI can tell our orders from anything else on the
//!    account at a glance.
//!
//! ## The rule that makes it a safety property
//!
//! **A cloid without the `MV` prefix in a `userFills` row is NOT
//! ours.** Count it, never book it against a slot. A fill the engine
//! did not order is exactly the evidence reconciliation exists to
//! catch, and quietly attributing it to a member would destroy that
//! evidence at the moment it appeared.

use core_config::exec::EXEC_SLOTS;

/// `'M'`, `'V'` — the marker that says an engine placed this order.
pub const MAGIC: [u8; 2] = [0x4D, 0x56];

/// Byte the slot lives in.
const SLOT_BYTE: usize = 2;

/// Where the member's own `client_oid` starts.
const OID_OFF: usize = 8;

/// Build the venue cloid for one slot's order.
///
/// `strategy_id` is masked into the slot space rather than rejected:
/// this is on the submit path, and a member with a nonsense slot byte
/// must produce a cloid that decodes to SOMETHING traceable rather
/// than a panic between the strategy and the venue. The router refuses
/// the order on the mode lookup long before here; this is belt.
#[inline(always)]
#[must_use]
pub fn encode(strategy_id: u8, client_oid: u64) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = MAGIC[0];
    c[1] = MAGIC[1];
    c[SLOT_BYTE] = strategy_id & (EXEC_SLOTS as u8 - 1);
    // Bytes 3..8 stay zero: reserved, and a non-zero one in a row we
    // receive is a cloid we did not write.
    c[OID_OFF..].copy_from_slice(&client_oid.to_be_bytes());
    c
}

/// What a cloid seen on the wire turned out to be.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Owner {
    /// This engine placed it, in this slot, with this client id.
    Ours {
        /// The slot that owns the fill.
        strategy_id: u8,
        /// The member's own id for the order.
        client_oid: u64,
    },
    /// Not ours. **Never book it against a slot.**
    Foreign,
}

/// Decode a cloid echoed by the venue.
///
/// Branchless in the common path and total: every one of the 2^128
/// possible inputs maps to exactly one answer, and `Foreign` is the
/// answer for all but a vanishing fraction of them.
#[inline(always)]
#[must_use]
pub fn decode(cloid: &[u8; 16]) -> Owner {
    // SAFETY-free: fixed-size array, every index is a constant below
    // its length, so there is no bounds check to elide and nothing to
    // get wrong.
    let magic_ok = (cloid[0] == MAGIC[0]) & (cloid[1] == MAGIC[1]);
    let slot = cloid[SLOT_BYTE];
    let slot_ok = (slot as usize) < EXEC_SLOTS;
    // The reserved bytes must be zero. A cloid with our magic but a
    // dirty reserve is not one we wrote, and treating it as ours would
    // be the one case where the marker gave false confidence.
    let mut reserved = 0u8;
    for b in &cloid[SLOT_BYTE + 1..OID_OFF] {
        reserved |= *b;
    }
    if !(magic_ok & slot_ok & (reserved == 0)) {
        return Owner::Foreign;
    }
    let mut oid = [0u8; 8];
    oid.copy_from_slice(&cloid[OID_OFF..]);
    Owner::Ours {
        strategy_id: slot,
        client_oid: u64::from_be_bytes(oid),
    }
}

/// Render a cloid the way the venue does: `0x` + 32 lowercase hex.
///
/// Zero-alloc: writes into a caller-owned buffer and returns the used
/// length, because this runs once per order on the submit path.
#[inline]
pub fn to_hex(cloid: &[u8; 16], dst: &mut [u8; 34]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    dst[0] = b'0';
    dst[1] = b'x';
    for (i, b) in cloid.iter().enumerate() {
        dst[2 + i * 2] = HEX[(b >> 4) as usize];
        dst[3 + i * 2] = HEX[(b & 0x0F) as usize];
    }
    34
}

/// Parse `0x`-prefixed 32-hex back to bytes. Used on the RECEIVE path,
/// where the input is the venue's and must be assumed hostile.
#[must_use]
pub fn from_hex(s: &[u8]) -> Option<[u8; 16]> {
    let s = if s.len() == 34 && s[0] == b'0' && (s[1] | 0x20) == b'x' {
        &s[2..]
    } else if s.len() == 32 {
        s
    } else {
        return None;
    };
    let mut out = [0u8; 16];
    for i in 0..16 {
        let hi = nib(s[i * 2])?;
        let lo = nib(s[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

#[inline(always)]
fn nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// The slot must fit in one byte and the mask above must be a mask.
const _: () = assert!(EXEC_SLOTS.is_power_of_two());
const _: () = assert!(EXEC_SLOTS <= 256);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cloid_round_trips_through_every_slot() {
        for slot in 0..EXEC_SLOTS as u8 {
            for oid in [0u64, 1, 42, u64::MAX, 0x0123_4567_89ab_cdef] {
                let c = encode(slot, oid);
                assert_eq!(
                    decode(&c),
                    Owner::Ours {
                        strategy_id: slot,
                        client_oid: oid
                    },
                    "slot {slot} oid {oid}"
                );
            }
        }
    }

    /// The layout is a wire format now: BIN15 is slot 3, and an order
    /// placed today must decode the same way after any refactor.
    #[test]
    fn the_layout_is_pinned_byte_for_byte() {
        let c = encode(3, 0x0102_0304_0506_0708);
        assert_eq!(
            c,
            [
                0x4D, 0x56, 0x03, 0, 0, 0, 0, 0, // magic, slot, reserved
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // client_oid BE
            ]
        );
        let mut hex = [0u8; 34];
        let n = to_hex(&c, &mut hex);
        assert_eq!(
            core::str::from_utf8(&hex[..n]).unwrap(),
            "0x4d560300000000000102030405060708"
        );
    }

    /// **The safety property.** Anything else on the account is not
    /// ours, and must never be attributed to a slot.
    #[test]
    fn a_foreign_cloid_is_never_attributed_to_a_slot() {
        // No magic at all — a hand-placed order, or another tool.
        assert_eq!(decode(&[0u8; 16]), Owner::Foreign);
        assert_eq!(decode(&[0xFF; 16]), Owner::Foreign);
        // Right magic, slot out of range.
        let mut c = encode(0, 7);
        c[SLOT_BYTE] = EXEC_SLOTS as u8;
        assert_eq!(decode(&c), Owner::Foreign);
        // Right magic, right slot, but a DIRTY reserved region. Our
        // encoder never writes that, so neither did we.
        for i in (SLOT_BYTE + 1)..OID_OFF {
            let mut c = encode(3, 7);
            c[i] = 1;
            assert_eq!(decode(&c), Owner::Foreign, "reserved byte {i} ignored");
        }
        // One bit off in the magic.
        let mut c = encode(3, 7);
        c[0] ^= 0x01;
        assert_eq!(decode(&c), Owner::Foreign);
        c = encode(3, 7);
        c[1] ^= 0x01;
        assert_eq!(decode(&c), Owner::Foreign);
    }

    /// Two members may both emit client_oid = 1. They must not both
    /// emit the same cloid — that is half the reason the slot is in
    /// there.
    #[test]
    fn two_slots_with_the_same_client_oid_do_not_collide() {
        let a = encode(1, 1);
        let b = encode(3, 1);
        assert_ne!(a, b);
        assert_eq!(
            decode(&a),
            Owner::Ours {
                strategy_id: 1,
                client_oid: 1
            }
        );
        assert_eq!(
            decode(&b),
            Owner::Ours {
                strategy_id: 3,
                client_oid: 1
            }
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_everything_else() {
        let c = encode(3, 0xdead_beef);
        let mut hex = [0u8; 34];
        let n = to_hex(&c, &mut hex);
        assert_eq!(from_hex(&hex[..n]), Some(c));
        // Bare 32 hex, no prefix, is accepted.
        assert_eq!(from_hex(&hex[2..n]), Some(c));
        // Uppercase X and uppercase digits.
        let upper: Vec<u8> = hex[..n].iter().map(|b| b.to_ascii_uppercase()).collect();
        assert_eq!(from_hex(&upper), Some(c));
        // Everything else is refused rather than guessed at.
        assert_eq!(from_hex(b""), None);
        assert_eq!(from_hex(b"0x"), None);
        assert_eq!(from_hex(&hex[..n - 1]), None);
        assert_eq!(from_hex(b"0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"), None);
    }

    /// A slot byte above the slot space is masked on the way OUT (so a
    /// buggy member still produces something traceable) but refused on
    /// the way IN (so a venue row cannot invent a slot).
    #[test]
    fn the_encoder_masks_and_the_decoder_refuses() {
        let c = encode(0xFF, 9);
        assert_eq!(c[SLOT_BYTE], (EXEC_SLOTS - 1) as u8);
        assert_eq!(
            decode(&c),
            Owner::Ours {
                strategy_id: (EXEC_SLOTS - 1) as u8,
                client_oid: 9
            }
        );
    }

    /// Decoding must be total: no input panics, and nothing that is
    /// not ours comes back as ours.
    #[test]
    fn decoding_is_total_over_arbitrary_bytes() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..200_000 {
            let mut c = [0u8; 16];
            for b in c.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            if let Owner::Ours { strategy_id, .. } = decode(&c) {
                assert_eq!([c[0], c[1]], MAGIC);
                assert!((strategy_id as usize) < EXEC_SLOTS);
                assert_eq!(&c[SLOT_BYTE + 1..OID_OFF], &[0u8; 5]);
            }
        }
    }
}
