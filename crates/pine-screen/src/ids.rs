// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The identifiers the files are keyed by.
//! COPY-DOCTRINE: load-time hashing of short keys; never reached by the bar loop.

use core_crypto::Sha256;

/// Everything that makes two tier-1 trials different, hashed into the trial id.
#[derive(Debug, Clone, Copy)]
pub struct TrialIdParts<'a> {
    /// SHA-256 of the `.pine` bytes.
    pub source_sha256: &'a [u8; 32],
    /// [`crate::screen::Loaded::input_digest`].
    pub input_digest: &'a [u8; 32],
    /// Canonical `venue:symbol` key of the chart.
    pub symbol_key: &'a str,
    /// Chart timeframe, seconds.
    pub tf_s: u32,
    /// The compile mode actually used.
    pub mode: pine_ir::ExecMode,
    /// Scoring window `[from_ms, to_ms)`, UTC day boundaries.
    pub from_ms: i64,
    /// Exclusive end of the scoring window.
    pub to_ms: i64,
    /// Selection notional, USD ×1e6.
    pub notional_1e6: i64,
    /// [`fee_digest`] of the flag set the trial was charged under.
    pub fee_digest: &'a [u8; 32],
    /// `BarFileHdr::src_digest64` of the chart file (a re-prepared file is a different trial).
    pub bars_digest64: u64,
}

/// `SHA-256("pvm7-trial-1\0" ‖ fields in declaration order, integers LE, the key's bytes then 0)`.
/// `pvm-select` may use its own ledger id; any 32 bytes key a [`crate::RowRec`].
// pin:07-trial-id-mode-byte: `mode` is hashed as its `repr(u8)` discriminant (one byte), the only
// integer encoding of a one-byte enum the spec's "integers LE" admits.
pub fn trial_id(p: &TrialIdParts<'_>) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"pvm7-trial-1\0");
    h.update(p.source_sha256);
    h.update(p.input_digest);
    h.update(p.symbol_key.as_bytes());
    h.update(&[0]);
    h.update(&p.tf_s.to_le_bytes());
    h.update(&[p.mode as u8]);
    h.update(&p.from_ms.to_le_bytes());
    h.update(&p.to_ms.to_le_bytes());
    h.update(&p.notional_1e6.to_le_bytes());
    h.update(p.fee_digest);
    h.update(&p.bars_digest64.to_le_bytes());
    h.finalize()
}

/// `SHA-256` of the `--fee-bps` flags exactly as given, each followed by `\n`, in order.
pub fn fee_digest(flags: &[&str]) -> [u8; 32] {
    let mut h = Sha256::new();
    let mut i = 0;
    while i < flags.len() {
        h.update(flags[i].as_bytes());
        h.update(b"\n");
        i += 1;
    }
    h.finalize()
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex of 32 bytes (file names: `<hex>.f64`, hand-off directories).
pub fn hex32(b: &[u8; 32]) -> [u8; 64] {
    let mut out = [0u8; 64];
    let mut i = 0;
    while i < 32 {
        out[2 * i] = HEX[(b[i] >> 4) as usize];
        out[2 * i + 1] = HEX[(b[i] & 0x0f) as usize];
        i += 1;
    }
    out
}

/// Inverse of [`hex32`]; `None` unless exactly 64 lowercase hex digits.
pub fn parse_hex32(s: &[u8]) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    };
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nib(s[2 * i])? << 4) | nib(s[2 * i + 1])?;
        i += 1;
    }
    Some(out)
}
