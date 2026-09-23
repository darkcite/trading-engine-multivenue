// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Protocol Buffers (proto3) wire primitives — MX1 (operator ruling O-MX3)
//!
//! The first non-JSON ingress payload in the engine is MEXC spot
//! (`wss://wbs-api.mexc.com/ws`, WS opcode BINARY,
//! `PushDataV3ApiWrapper`). This module is the whole decoder: a
//! base-128 varint reader and a forward field walk. There is no
//! protobuf library, no codegen and no intermediate representation —
//! a length-delimited field is `tag-varint | len-varint | bytes`, so
//! its payload is returned as a `(start, end)` span into the caller's
//! buffer and scanned IN PLACE (MEXC renders every price and quantity
//! as an ASCII decimal string inside the message, which feeds
//! [`crate::scan_price_1e6`] unchanged). Zero allocation, zero copy.
//!
//! ## Laws
//!
//! * Every accessor returns `Option` — no bounds-check panics, no
//!   loops that a malformed varint can stall (a varint is at most
//!   [`PB_VARINT_MAX_LEN`] bytes; the 10th byte may carry one bit).
//! * Wire types 0 (varint), 1 (fixed64), 2 (length-delimited) and
//!   5 (fixed32) are decoded; the deprecated groups (3/4) and the
//!   undefined 6/7 are rejected outright; field number 0 is invalid.
//! * Skipping a field is O(1) — a length-delimited body is jumped by
//!   its length, never descended — so, unlike
//!   [`crate::skip_json_value`], no depth cap exists or is needed.
//!   The plan's `skip_pb_field` is [`scan_pb_field`]`(..)?.end`: one
//!   primitive, not two.
//! * proto3 permits ANY field order and omits default values: a
//!   message walker built on these primitives is an order-agnostic
//!   forward walk that tolerates unknown field numbers (forward
//!   compatibility — plan §4 D5).

use crate::Pos;

/// Longest valid base-128 varint (a full `u64`).
pub const PB_VARINT_MAX_LEN: usize = 10;

/// Largest valid field number (`2^29 - 1`).
pub const PB_FIELD_NO_MAX: u32 = (1 << 29) - 1;

/// Wire type 0 — base-128 varint.
pub const PB_WT_VARINT: u8 = 0;
/// Wire type 1 — little-endian fixed 64-bit.
pub const PB_WT_I64: u8 = 1;
/// Wire type 2 — length-delimited (string, bytes, sub-message,
/// packed repeated).
pub const PB_WT_LEN: u8 = 2;
/// Wire type 5 — little-endian fixed 32-bit.
pub const PB_WT_I32: u8 = 5;

/// One decoded field header + value location. `Copy` POD; spans index
/// the buffer passed to [`scan_pb_field`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PbField {
    /// Field number (`1..=PB_FIELD_NO_MAX`).
    pub field_no: u32,
    /// Wire type (one of the four `PB_WT_*` constants).
    pub wire_type: u8,
    /// [`PB_WT_VARINT`]: the decoded value; [`PB_WT_I64`] /
    /// [`PB_WT_I32`]: the little-endian fixed value; [`PB_WT_LEN`]:
    /// the payload length in bytes.
    pub value: u64,
    /// First byte of the value (for [`PB_WT_LEN`], the payload).
    pub start: Pos,
    /// One past the field's last byte — the next field's tag
    /// position (for [`PB_WT_LEN`], also the payload end).
    pub end: Pos,
}

/// Decode one base-128 varint at `pos`. Returns `(value, new_pos)`;
/// `None` when the buffer ends inside the varint, when it runs past
/// [`PB_VARINT_MAX_LEN`] bytes, or when the 10th byte overflows `u64`.
/// Non-canonical (over-long, zero-padded) encodings decode — protobuf
/// parsers accept them, and so must a venue decoder.
#[inline]
pub fn scan_varint(buf: &[u8], pos: Pos) -> Option<(u64, Pos)> {
    // Fast path: the tag bytes and most lengths are one byte.
    let b0 = *buf.get(pos)?;
    if b0 & 0x80 == 0 {
        return Some((b0 as u64, pos + 1));
    }
    let mut v = (b0 & 0x7F) as u64;
    let mut shift: u32 = 7;
    let mut i = pos + 1;
    while shift < 64 {
        let b = *buf.get(i)?;
        i += 1;
        // Byte 10 (shift 63) may carry only bit 0 and no continuation.
        if shift == 63 && b > 1 {
            return None;
        }
        v |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((v, i));
        }
        shift += 7;
    }
    None
}

/// Decode one field TAG at `pos`: `(field_no, wire_type, new_pos)`.
/// `None` on a malformed varint, a key wider than `u32`, field number
/// 0, a group wire type (3/4) or an undefined one (6/7).
#[inline]
pub fn scan_pb_tag(buf: &[u8], pos: Pos) -> Option<(u32, u8, Pos)> {
    let (key, next) = scan_varint(buf, pos)?;
    if key > u32::MAX as u64 {
        return None;
    }
    let field_no = (key >> 3) as u32;
    let wire_type = (key & 7) as u8;
    if field_no == 0 {
        return None;
    }
    match wire_type {
        PB_WT_VARINT | PB_WT_I64 | PB_WT_LEN | PB_WT_I32 => Some((field_no, wire_type, next)),
        _ => None,
    }
}

/// Decode a length prefix at `pos` and bound its payload. Returns
/// `(start, end)` of the payload; `None` when the length is malformed
/// or the payload runs past `buf`.
#[inline]
pub fn scan_pb_len(buf: &[u8], pos: Pos) -> Option<(Pos, Pos)> {
    let (len, start) = scan_varint(buf, pos)?;
    let len = usize::try_from(len).ok()?;
    let end = start.checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    Some((start, end))
}

/// Decode one complete field (tag + value) at `pos`. The returned
/// [`PbField::end`] is where the next field's tag starts, so a message
/// walk is `while pos < msg.len() { let f = scan_pb_field(msg, pos)?;
/// …; pos = f.end; }` — and skipping an unwanted field is simply not
/// looking at it.
#[inline]
pub fn scan_pb_field(buf: &[u8], pos: Pos) -> Option<PbField> {
    let (field_no, wire_type, vpos) = scan_pb_tag(buf, pos)?;
    let (value, start, end) = match wire_type {
        PB_WT_VARINT => {
            let (v, end) = scan_varint(buf, vpos)?;
            (v, vpos, end)
        }
        PB_WT_LEN => {
            let (start, end) = scan_pb_len(buf, vpos)?;
            ((end - start) as u64, start, end)
        }
        PB_WT_I64 => {
            let end = vpos.checked_add(8)?;
            let bytes: [u8; 8] = buf.get(vpos..end)?.try_into().ok()?;
            (u64::from_le_bytes(bytes), vpos, end)
        }
        // scan_pb_tag admits only the four decoded wire types.
        _ => {
            debug_assert_eq!(wire_type, PB_WT_I32);
            let end = vpos.checked_add(4)?;
            let bytes: [u8; 4] = buf.get(vpos..end)?.try_into().ok()?;
            (u32::from_le_bytes(bytes) as u64, vpos, end)
        }
    };
    Some(PbField {
        field_no,
        wire_type,
        value,
        start,
        end,
    })
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

/// Reference ENCODERS for the tests (allocating; test-only).
#[cfg(test)]
pub(crate) mod enc {
    pub(crate) fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    pub(crate) fn tag(out: &mut Vec<u8>, field_no: u32, wire_type: u8) {
        varint(out, ((field_no as u64) << 3) | wire_type as u64);
    }

    pub(crate) fn len_field(out: &mut Vec<u8>, field_no: u32, payload: &[u8]) {
        tag(out, field_no, super::PB_WT_LEN);
        varint(out, payload.len() as u64);
        out.extend_from_slice(payload);
    }

    pub(crate) fn varint_field(out: &mut Vec<u8>, field_no: u32, v: u64) {
        tag(out, field_no, super::PB_WT_VARINT);
        varint(out, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_known_encodings() {
        assert_eq!(scan_varint(&[0x00], 0), Some((0, 1)));
        assert_eq!(scan_varint(&[0x7F], 0), Some((127, 1)));
        assert_eq!(scan_varint(&[0x96, 0x01], 0), Some((150, 2)));
        // u64::MAX: nine 0xFF then 0x01.
        let max = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        assert_eq!(scan_varint(&max, 0), Some((u64::MAX, 10)));
        // Offset start + trailing bytes untouched.
        assert_eq!(scan_varint(&[0xAA, 0x96, 0x01, 0x05], 1), Some((150, 3)));
        // Non-canonical (over-long) encodings decode.
        assert_eq!(scan_varint(&[0x80, 0x00], 0), Some((0, 2)));
    }

    #[test]
    fn varint_failure_modes() {
        assert_eq!(scan_varint(&[], 0), None, "empty");
        assert_eq!(scan_varint(&[0x80], 0), None, "ends inside");
        assert_eq!(scan_varint(&[0x01], 1), None, "pos at end");
        assert_eq!(scan_varint(&[0x01], 7), None, "pos past end");
        let overflow = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02];
        assert_eq!(scan_varint(&overflow, 0), None, "10th byte > 1");
        let eleven = [0x80; 11];
        assert_eq!(scan_varint(&eleven, 0), None, "longer than 10 bytes");
    }

    #[test]
    fn tag_decodes_the_mexc_body_tags() {
        // Plan §1.1: verified-live oneof tags.
        assert_eq!(scan_pb_tag(&[0xDA, 0x13], 0), Some((315, PB_WT_LEN, 2)));
        assert_eq!(scan_pb_tag(&[0xD2, 0x13], 0), Some((314, PB_WT_LEN, 2)));
        assert_eq!(scan_pb_tag(&[0xEA, 0x12], 0), Some((301, PB_WT_LEN, 2)));
        assert_eq!(scan_pb_tag(&[0x0A], 0), Some((1, PB_WT_LEN, 1)));
        assert_eq!(scan_pb_tag(&[0x30], 0), Some((6, PB_WT_VARINT, 1)));
    }

    #[test]
    fn tag_rejects_invalid_keys() {
        assert_eq!(scan_pb_tag(&[0x02], 0), None, "field 0");
        assert_eq!(scan_pb_tag(&[0x0B], 0), None, "group start (3)");
        assert_eq!(scan_pb_tag(&[0x0C], 0), None, "group end (4)");
        assert_eq!(scan_pb_tag(&[0x0E], 0), None, "wire type 6");
        assert_eq!(scan_pb_tag(&[0x0F], 0), None, "wire type 7");
        // Key wider than u32.
        let mut wide = Vec::new();
        enc::varint(&mut wide, (u32::MAX as u64) + 1);
        assert_eq!(scan_pb_tag(&wide, 0), None);
        assert_eq!(scan_pb_tag(&[], 0), None);
    }

    #[test]
    fn len_bounds_the_payload() {
        assert_eq!(scan_pb_len(&[0x03, b'a', b'b', b'c'], 0), Some((1, 4)));
        assert_eq!(scan_pb_len(&[0x00], 0), Some((1, 1)), "empty payload");
        assert_eq!(scan_pb_len(&[0x04, b'a', b'b', b'c'], 0), None, "short");
        // A length near usize::MAX must not overflow the end.
        let huge = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        assert_eq!(scan_pb_len(&huge, 0), None);
    }

    #[test]
    fn field_decodes_every_wire_type() {
        let mut m = Vec::new();
        enc::varint_field(&mut m, 6, 1_789_897_581_009);
        enc::len_field(&mut m, 1, b"80535.88");
        enc::tag(&mut m, 9, PB_WT_I64);
        m.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        enc::tag(&mut m, 10, PB_WT_I32);
        m.extend_from_slice(&0xA1B2_C3D4u32.to_le_bytes());

        let f = scan_pb_field(&m, 0).unwrap();
        assert_eq!((f.field_no, f.wire_type, f.value), (6, PB_WT_VARINT, 1_789_897_581_009));
        let g = scan_pb_field(&m, f.end).unwrap();
        assert_eq!((g.field_no, g.wire_type, g.value), (1, PB_WT_LEN, 8));
        assert_eq!(&m[g.start..g.end], b"80535.88");
        let h = scan_pb_field(&m, g.end).unwrap();
        assert_eq!((h.field_no, h.wire_type, h.value), (9, PB_WT_I64, 0x0102_0304_0506_0708));
        let k = scan_pb_field(&m, h.end).unwrap();
        assert_eq!((k.field_no, k.wire_type, k.value), (10, PB_WT_I32, 0xA1B2_C3D4));
        assert_eq!(k.end, m.len());
        assert_eq!(scan_pb_field(&m, k.end), None, "end of message");
    }

    #[test]
    fn field_rejects_truncation_in_every_wire_type() {
        let mut v = Vec::new();
        enc::tag(&mut v, 1, PB_WT_VARINT);
        v.push(0x80); // varint cut
        assert_eq!(scan_pb_field(&v, 0), None);
        let mut l = Vec::new();
        enc::tag(&mut l, 1, PB_WT_LEN);
        l.extend_from_slice(&[0x05, b'x']); // 5 promised, 1 present
        assert_eq!(scan_pb_field(&l, 0), None);
        let mut q = Vec::new();
        enc::tag(&mut q, 1, PB_WT_I64);
        q.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]); // 7 of 8
        assert_eq!(scan_pb_field(&q, 0), None);
        let mut d = Vec::new();
        enc::tag(&mut d, 1, PB_WT_I32);
        d.extend_from_slice(&[1, 2, 3]); // 3 of 4
        assert_eq!(scan_pb_field(&d, 0), None);
    }

    #[test]
    fn a_wrapper_frame_walks_in_any_field_order() {
        // PushDataV3ApiWrapper shape (plan §1.1) with the body FIRST
        // and the channel LAST — proto3 permits any order (§4 D5).
        let mut body = Vec::new();
        enc::len_field(&mut body, 1, b"80535.88");
        enc::len_field(&mut body, 3, b"80535.89");
        let mut frame = Vec::new();
        enc::len_field(&mut frame, 315, &body);
        enc::len_field(&mut frame, 3, b"BTCUSDT");
        enc::varint_field(&mut frame, 6, 1_789_897_581_013);
        enc::len_field(&mut frame, 1, b"spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT");

        let mut pos = 0;
        let mut seen = [0u32; 4];
        let mut n = 0;
        while pos < frame.len() {
            let f = scan_pb_field(&frame, pos).unwrap();
            seen[n] = f.field_no;
            n += 1;
            pos = f.end;
        }
        assert_eq!(seen, [315, 3, 6, 1]);
        assert_eq!(pos, frame.len());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn varint_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..=32), pos in 0usize..40) {
            if let Some((_, end)) = scan_varint(&buf, pos) {
                prop_assert!(end > pos && end <= buf.len() && end - pos <= PB_VARINT_MAX_LEN);
            }
        }

        #[test]
        fn varint_roundtrips_the_reference_encoder(v in any::<u64>(), pad in proptest::collection::vec(any::<u8>(), 0..8)) {
            let mut buf = Vec::new();
            enc::varint(&mut buf, v);
            let n = buf.len();
            buf.extend_from_slice(&pad);
            prop_assert_eq!(scan_varint(&buf, 0), Some((v, n)));
        }

        #[test]
        fn tag_roundtrips(field_no in 1u32..=PB_FIELD_NO_MAX, wt_idx in 0usize..4) {
            let wt = [PB_WT_VARINT, PB_WT_I64, PB_WT_LEN, PB_WT_I32][wt_idx];
            let mut buf = Vec::new();
            enc::tag(&mut buf, field_no, wt);
            prop_assert_eq!(scan_pb_tag(&buf, 0), Some((field_no, wt, buf.len())));
        }

        #[test]
        fn field_walk_never_panics_and_always_progresses(buf in proptest::collection::vec(any::<u8>(), 0..=512)) {
            let mut pos = 0usize;
            while pos < buf.len() {
                match scan_pb_field(&buf, pos) {
                    Some(f) => {
                        prop_assert!(f.end > pos, "a field always consumes bytes");
                        prop_assert!(f.end <= buf.len());
                        prop_assert!(f.start <= f.end);
                        pos = f.end;
                    }
                    None => break,
                }
            }
        }
    }
}
