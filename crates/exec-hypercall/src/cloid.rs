// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! LAW E-9 for Hypercall: the client id carries the slot.
//!
//! `client_id` is a free string at the venue. Ours is exactly
//! [`CLOID_LEN`] lowercase hex characters — 16 bytes:
//!
//! ```text
//! 'H' 'C' | version | slot | client_oid (8, big-endian) | check (4)
//! ```
//!
//! The check is FNV-1a over the first 12 bytes, so an id someone else
//! chose (the web app, another tool on the same wallet) does not decode
//! as ours by accident: [`decode`] refuses anything but our exact
//! shape. Nothing in it leaks intent beyond the slot tag (plan D11).

/// Characters in a rendered client id.
pub const CLOID_LEN: usize = 32;

const MAGIC: [u8; 2] = *b"HC";
const VERSION: u8 = 1;

fn fnv1a32(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    let mut i = 0usize;
    while i < b.len() {
        h ^= u32::from(b[i]);
        h = h.wrapping_mul(0x0100_0193);
        i += 1;
    }
    h
}

/// Render `(slot, client_oid)` as the client id, in place.
#[must_use]
pub fn encode(slot: u8, client_oid: u64) -> [u8; CLOID_LEN] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut raw = [0u8; 16];
    raw[0] = MAGIC[0];
    raw[1] = MAGIC[1];
    raw[2] = VERSION;
    raw[3] = slot;
    let oid = client_oid.to_be_bytes();
    let mut i = 0usize;
    while i < 8 {
        raw[4 + i] = oid[i];
        i += 1;
    }
    let ck = fnv1a32(&raw[..12]).to_be_bytes();
    raw[12] = ck[0];
    raw[13] = ck[1];
    raw[14] = ck[2];
    raw[15] = ck[3];
    let mut out = [0u8; CLOID_LEN];
    let mut j = 0usize;
    while j < 16 {
        out[2 * j] = HEX[(raw[j] >> 4) as usize];
        out[2 * j + 1] = HEX[(raw[j] & 0x0f) as usize];
        j += 1;
    }
    out
}

fn lower_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// `(slot, client_oid)` of one of OUR client ids; `None` for anything
/// else (wrong length, case, magic, version or check).
#[must_use]
pub fn decode(s: &[u8]) -> Option<(u8, u64)> {
    if s.len() != CLOID_LEN {
        return None;
    }
    let mut raw = [0u8; 16];
    let mut j = 0usize;
    while j < 16 {
        raw[j] = (lower_nibble(s[2 * j])? << 4) | lower_nibble(s[2 * j + 1])?;
        j += 1;
    }
    if raw[0] != MAGIC[0] || raw[1] != MAGIC[1] || raw[2] != VERSION {
        return None;
    }
    let ck = fnv1a32(&raw[..12]).to_be_bytes();
    if raw[12..16] != ck {
        return None;
    }
    let mut oid = [0u8; 8];
    let mut i = 0usize;
    while i < 8 {
        oid[i] = raw[4 + i];
        i += 1;
    }
    Some((raw[3], u64::from_be_bytes(oid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_trip_keeps_slot_and_id() {
        let c = encode(7, 0x0123_4567_89ab_cdef);
        assert_eq!(decode(&c), Some((7, 0x0123_4567_89ab_cdef)));
        assert!(c.iter().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(&c[..8], b"48430107", "magic, version, slot");
    }

    #[test]
    fn foreign_and_damaged_ids_are_refused() {
        let mut c = encode(7, 42);
        assert!(decode(b"client-123").is_none());
        assert!(decode(&c[..31]).is_none());
        c[20] = if c[20] == b'0' { b'1' } else { b'0' };
        assert!(decode(&c).is_none(), "the check catches a flipped digit");
        // Upper case is not our spelling: 0x7f's `f` upper-cased.
        let mut up = encode(7, 0x7f);
        let at = up.iter().position(u8::is_ascii_alphabetic).expect("a hex letter");
        up[at] = up[at].to_ascii_uppercase();
        assert!(decode(&up).is_none());
    }

    #[test]
    fn distinct_slots_and_ids_render_distinct_strings() {
        assert_ne!(encode(7, 1), encode(6, 1));
        assert_ne!(encode(7, 1), encode(7, 2));
    }
}
