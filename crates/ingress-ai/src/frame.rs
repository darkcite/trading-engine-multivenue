// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! 82-byte UDS wire frame (design §4.1) + per-connection sequence
//! policy (design §4.4 step 5).
//!
//! Frame layout:
//!
//! ```text
//! [len: u16 LE = 80] [AiCmd: 64 B] [tag: 16 B]
//! total = 82 B; tag = HMAC-SHA256(AI_INGRESS_HMAC_KEY, cmd_bytes)[0..16]
//! ```
//!
//! Everything in this module is pure and I/O-free — it is the surface
//! the `ai_cmd_frame` fuzz target and the property tests drive.
//! Zero-copy accounting (doctrine): [`parse_frame`] reads the command
//! bytes in place from the caller's frame view; the single documented
//! copy is the 64-B stack materialization inside
//! [`core_types::AiCmd::read_le`] (unaligned source — see its docs).

use core_types::AiCmd;
use core_types::AiCmdShapeError;

/// Total frame length on the wire: 2 (len) + 64 (cmd) + 16 (tag).
pub const FRAME_LEN: usize = 82;

/// The only legal value of the leading `len` field: 64 + 16.
pub const LEN_FIELD_VALUE: u16 = 80;

/// Byte offset of the 64-B command inside a frame.
pub const CMD_OFFSET: usize = 2;

/// Length of the command payload.
pub const CMD_LEN: usize = 64;

/// Byte offset of the HMAC tag inside a frame.
pub const TAG_OFFSET: usize = CMD_OFFSET + CMD_LEN;

/// Length of the truncated HMAC-SHA256 tag.
pub const TAG_LEN: usize = 16;

// The tag length is pinned by core-crypto; a drift would silently
// break every frame on the wire.
const _: () = assert!(TAG_LEN == core_crypto::HMAC_TAG16_LEN);
const _: () = assert!(FRAME_LEN == CMD_OFFSET + CMD_LEN + TAG_LEN);
const _: () = assert!(LEN_FIELD_VALUE as usize == CMD_LEN + TAG_LEN);

/// Why a full 82-B frame was refused (§4.4 steps 1/3/4).
///
/// The variants map onto counters and connection policy:
/// `BadLen`/`BadTag` are connection-fatal (`protocol_err_total` /
/// `hmac_fail_total`, drop conn); `Malformed` discards the frame but
/// keeps the connection (`malformed_total`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Leading `len` field is not [`LEN_FIELD_VALUE`].
    BadLen(u16),
    /// HMAC tag mismatch (constant-time compare failed).
    BadTag,
    /// Command bytes violate the per-kind shape table (design §3).
    Malformed(AiCmdShapeError),
}

/// Read the leading `len` field of a (possibly partial) frame.
/// Caller guarantees `buf.len() >= 2`; enforced by `debug_assert!`.
#[inline(always)]
pub fn len_field(buf: &[u8]) -> u16 {
    debug_assert!(buf.len() >= 2, "len_field needs at least 2 bytes");
    u16::from_le_bytes([buf[0], buf[1]])
}

/// Validate and materialize one full 82-B frame (§4.4 steps 1, 3, 4).
///
/// Order matters and is fixed by design §4.4: length check, then HMAC
/// verify over the raw command bytes (constant-time compare — no
/// partial trust), then shape validation of the materialized command.
/// The sequence policy (step 5) is stateful and applied by the caller
/// via [`SeqPolicy::admit`].
#[inline]
pub fn parse_frame(key: &[u8; 32], frame: &[u8; FRAME_LEN]) -> Result<AiCmd, FrameError> {
    let lf = len_field(frame);
    if lf != LEN_FIELD_VALUE {
        return Err(FrameError::BadLen(lf));
    }
    let tag = core_crypto::hmac_sha256_tag16(key, &frame[CMD_OFFSET..TAG_OFFSET]);
    if !core_crypto::ct_eq(&tag, &frame[TAG_OFFSET..FRAME_LEN]) {
        return Err(FrameError::BadTag);
    }
    // SAFETY: `frame` is exactly FRAME_LEN (82) bytes by type;
    // CMD_OFFSET + CMD_LEN == 66 <= 82 (const-asserted above), so the
    // cast stays in bounds. `[u8; 64]` has alignment 1 — any offset is
    // valid. Lifetime is tied to `frame` via the reborrow.
    let cmd_bytes: &[u8; CMD_LEN] = unsafe { &*frame.as_ptr().add(CMD_OFFSET).cast() };
    let cmd = AiCmd::read_le(cmd_bytes);
    match cmd.validate_shape() {
        Ok(()) => Ok(cmd),
        Err(e) => Err(FrameError::Malformed(e)),
    }
}

/// Pack `cmd` into a full 82-B frame under `key`.
///
/// Byte-for-byte the writer-side counterpart of [`parse_frame`]; the
/// loopback integration suite and (later, item 9) the Python
/// `frames.py` golden vectors are pinned against it. Test/loopback
/// surface — production Rust only parses.
#[inline]
pub fn pack_frame(key: &[u8; 32], cmd: &AiCmd, out: &mut [u8; FRAME_LEN]) {
    out[0..CMD_OFFSET].copy_from_slice(&LEN_FIELD_VALUE.to_le_bytes());
    // SAFETY: AiCmd is `AsBytes` (repr(C), Copy, no padding holes —
    // asserted in core-types): every one of its 64 bytes is
    // initialized, so viewing it as raw bytes is defined behavior.
    let cmd_bytes: &[u8; CMD_LEN] = unsafe { &*(cmd as *const AiCmd).cast::<[u8; CMD_LEN]>() };
    const _: () = assert!(::core::mem::size_of::<AiCmd>() == CMD_LEN);
    out[CMD_OFFSET..TAG_OFFSET].copy_from_slice(cmd_bytes);
    let tag = core_crypto::hmac_sha256_tag16(key, cmd_bytes);
    out[TAG_OFFSET..FRAME_LEN].copy_from_slice(&tag);
}

// ---------------------------------------------------------------
// Sequence policy (§4.4 step 5)
// ---------------------------------------------------------------

/// Verdict of [`SeqPolicy::admit`] for one frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SeqVerdict {
    /// In order (or the first frame of the connection) — accept.
    Accept,
    /// Forward jump — accept, but count one gap event
    /// (`seq_gap_total` counts *events*, not missing frames; the gap
    /// width is carried for the caller's logs/tests).
    AcceptGap(u32),
    /// `seq <= last` — discard the frame (`seq_regress_total`).
    Regress,
}

/// Per-connection sequence tracker.
///
/// The worker's seq is strictly increasing *per session* (design §3).
/// A fresh connection primes on its first frame without counting a
/// gap, so both worker schemes — restart-at-1 and the persistent
/// SQLite allocator that survives reconnects — pass cleanly. The
/// listener constructs a fresh `SeqPolicy` per accepted connection.
#[derive(Copy, Clone, Debug)]
pub struct SeqPolicy {
    last: u32,
    primed: bool,
}

impl SeqPolicy {
    /// Fresh, unprimed tracker.
    #[inline]
    pub const fn new() -> Self {
        Self {
            last: 0,
            primed: false,
        }
    }

    /// Apply §4.4 step 5 to one frame's `seq`. Regressions do not
    /// update the high-water mark.
    #[inline]
    pub fn admit(&mut self, seq: u32) -> SeqVerdict {
        if !self.primed {
            self.primed = true;
            self.last = seq;
            return SeqVerdict::Accept;
        }
        if seq <= self.last {
            return SeqVerdict::Regress;
        }
        let gap = seq - self.last - 1;
        self.last = seq;
        if gap == 0 {
            SeqVerdict::Accept
        } else {
            SeqVerdict::AcceptGap(gap)
        }
    }

    /// High-water mark (0 before the first frame).
    #[inline]
    pub const fn last(&self) -> u32 {
        self.last
    }
}

impl Default for SeqPolicy {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{AiCmdKind, VenueId, AI_SIDE_NONE, STRATEGY_SLOT_NONE, SYMBOL_ID_NONE};

    const KEY: [u8; 32] = [0x42; 32];

    fn heartbeat(seq: u32) -> AiCmd {
        AiCmd::new(
            77,
            seq,
            SYMBOL_ID_NONE,
            0,
            0,
            0,
            AiCmdKind::Heartbeat,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    #[test]
    fn pack_then_parse_roundtrips() {
        let cmd = heartbeat(9);
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &cmd, &mut f);
        assert_eq!(len_field(&f), LEN_FIELD_VALUE);
        let back = parse_frame(&KEY, &f).unwrap();
        assert_eq!(back.seq, 9);
        assert_eq!(back.ts_ns, 77);
        assert_eq!(back.kind, AiCmdKind::Heartbeat.to_u8());
    }

    #[test]
    fn parse_rejects_bad_len() {
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &heartbeat(1), &mut f);
        f[0] = 81;
        assert_eq!(parse_frame(&KEY, &f).unwrap_err(), FrameError::BadLen(81));
    }

    #[test]
    fn parse_rejects_bad_tag_before_shape() {
        // A frame that is BOTH malformed and mis-tagged must report
        // the tag failure — §4.4 order: HMAC precedes shape.
        let mut cmd = heartbeat(1);
        cmd.px = 123; // shape violation for Heartbeat
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &cmd, &mut f);
        f[TAG_OFFSET] ^= 0xFF;
        assert_eq!(parse_frame(&KEY, &f).unwrap_err(), FrameError::BadTag);
    }

    #[test]
    fn parse_rejects_wrong_key() {
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &heartbeat(1), &mut f);
        let other = [0x43u8; 32];
        assert_eq!(parse_frame(&other, &f).unwrap_err(), FrameError::BadTag);
    }

    #[test]
    fn parse_reports_malformed_after_valid_tag() {
        let mut cmd = heartbeat(1);
        cmd.px = 123;
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &cmd, &mut f);
        assert!(matches!(
            parse_frame(&KEY, &f),
            Err(FrameError::Malformed(AiCmdShapeError::BadPx(123)))
        ));
    }

    #[test]
    fn tag_flip_of_every_byte_fails() {
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &heartbeat(1), &mut f);
        let mut i = TAG_OFFSET;
        while i < FRAME_LEN {
            let mut g = f;
            g[i] ^= 1;
            assert_eq!(parse_frame(&KEY, &g).unwrap_err(), FrameError::BadTag);
            i += 1;
        }
    }

    #[test]
    fn cmd_byte_flip_fails_tag() {
        let mut f = [0u8; FRAME_LEN];
        pack_frame(&KEY, &heartbeat(1), &mut f);
        let mut g = f;
        g[CMD_OFFSET + 8] ^= 1; // seq byte
        assert_eq!(parse_frame(&KEY, &g).unwrap_err(), FrameError::BadTag);
    }

    // ---- SeqPolicy ----

    #[test]
    fn seq_primes_on_any_first_value_without_gap() {
        let mut s = SeqPolicy::new();
        assert_eq!(s.admit(40_001), SeqVerdict::Accept);
        assert_eq!(s.last(), 40_001);
    }

    #[test]
    fn seq_accepts_in_order_counts_gaps_discards_regress() {
        let mut s = SeqPolicy::new();
        assert_eq!(s.admit(1), SeqVerdict::Accept);
        assert_eq!(s.admit(2), SeqVerdict::Accept);
        assert_eq!(s.admit(5), SeqVerdict::AcceptGap(2));
        assert_eq!(s.admit(5), SeqVerdict::Regress);
        assert_eq!(s.admit(3), SeqVerdict::Regress);
        // Regress must not move the high-water mark.
        assert_eq!(s.last(), 5);
        assert_eq!(s.admit(6), SeqVerdict::Accept);
    }

    #[test]
    fn seq_default_is_new() {
        let s = SeqPolicy::default();
        assert_eq!(s.last(), 0);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use core_types::{AiCmdKind, VenueId, AI_SIDE_NONE, STRATEGY_SLOT_NONE, SYMBOL_ID_NONE};
    use proptest::prelude::*;

    const KEY: [u8; 32] = [0x11; 32];

    proptest! {
        /// §11: arbitrary bytes never panic the parser.
        #[test]
        fn parse_never_panics_on_arbitrary_frames(bytes in proptest::array::uniform32(any::<u8>()), rest in proptest::collection::vec(any::<u8>(), FRAME_LEN - 32)) {
            let mut f = [0u8; FRAME_LEN];
            f[..32].copy_from_slice(&bytes);
            f[32..].copy_from_slice(&rest);
            let _ = parse_frame(&KEY, &f);
        }

        /// §11: pack→parse round-trip for shape-valid commands.
        #[test]
        fn pack_parse_roundtrip_heartbeats(seq in any::<u32>(), ts in any::<u64>()) {
            let cmd = AiCmd::new(
                ts, seq, SYMBOL_ID_NONE, 0, 0, 0,
                AiCmdKind::Heartbeat, VenueId::Ai,
                STRATEGY_SLOT_NONE, AI_SIDE_NONE, 0, 0,
            );
            let mut f = [0u8; FRAME_LEN];
            pack_frame(&KEY, &cmd, &mut f);
            let back = parse_frame(&KEY, &f).unwrap();
            prop_assert_eq!(back.seq, seq);
            prop_assert_eq!(back.ts_ns, ts);
        }

        /// Any single-bit corruption anywhere in cmd or tag bytes must
        /// fail the constant-time tag compare (or, for the len field,
        /// the length check) — never produce a different valid command.
        #[test]
        fn bit_flips_never_yield_a_valid_frame(pos in 0usize..FRAME_LEN, bit in 0u8..8) {
            let cmd = AiCmd::new(
                1, 2, SYMBOL_ID_NONE, 0, 0, 0,
                AiCmdKind::Heartbeat, VenueId::Ai,
                STRATEGY_SLOT_NONE, AI_SIDE_NONE, 0, 0,
            );
            let mut f = [0u8; FRAME_LEN];
            pack_frame(&KEY, &cmd, &mut f);
            f[pos] ^= 1 << bit;
            prop_assert!(parse_frame(&KEY, &f).is_err());
        }
    }
}
