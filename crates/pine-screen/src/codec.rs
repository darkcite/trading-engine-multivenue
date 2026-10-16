// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The little-endian on-disk codecs of the crate's fixed records: the bar-file header and row
//! (contract §3.16.3), the rows-file header and the row record (§3.6, contract §3.16.1). They live
//! here, apart from the types (`bars`, `rows`), so the modules the bar loop reaches carry no copy.
//!
//! COPY-DOCTRINE: cold codecs only — prepare time (`write_bar_file`, the digest check of
//! `BarFile::open`) and report time (`RowsWriter::append`, `RowsFile::read`). Each encoder renders a
//! field once into the FINAL record buffer (the one `write_all` / the hasher reads); each decoder
//! reads integers in place and copies a byte-array field once into the owned record the frozen
//! §3.6 signatures return. `run_group` and everything its loop calls never reach this module.

use crate::bars::BarFileHdr;
use crate::rows::{RowRec, RowsHdr};
use pine_ir::{Bar, TrialSummary};

/// `u32` at `b[o..o + 4]`, little-endian, read in place.
#[inline]
fn le_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// `u64` at `b[o..o + 8]`, little-endian, read in place.
#[inline]
fn le_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}

impl BarFileHdr {
    /// The on-disk (little-endian) bytes, field by field.
    // pin:07-hdr-le-bytes: §3.3 names "header encode/decode" (T4) without a signature; this is
    // RowRec's `to_le_bytes` / `from_le_bytes` shape (§3.6).
    pub fn to_le_bytes(self) -> [u8; 64] {
        let mut o = [0u8; 64];
        o[0..8].copy_from_slice(&self.magic);
        o[8..12].copy_from_slice(&self.version.to_le_bytes());
        o[12..16].copy_from_slice(&self.tf_s.to_le_bytes());
        o[16..24].copy_from_slice(&self.symbol_hash.to_le_bytes());
        o[24..32].copy_from_slice(&self.first_open_ms.to_le_bytes());
        o[32..40].copy_from_slice(&self.last_open_ms.to_le_bytes());
        o[40..48].copy_from_slice(&self.n.to_le_bytes());
        o[48..52].copy_from_slice(&self.holes.to_le_bytes());
        o[52] = self.venue;
        o[53] = self.source;
        o[54..56].copy_from_slice(&self._pad);
        o[56..64].copy_from_slice(&self.src_digest64.to_le_bytes());
        o
    }

    /// Decode the on-disk bytes, field by field (no validation).
    pub fn from_le_bytes(b: &[u8; 64]) -> BarFileHdr {
        BarFileHdr {
            magic: [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]],
            version: le_u32(b, 8),
            tf_s: le_u32(b, 12),
            symbol_hash: le_u64(b, 16),
            first_open_ms: le_u64(b, 24) as i64,
            last_open_ms: le_u64(b, 32) as i64,
            n: le_u64(b, 40),
            holes: le_u32(b, 48),
            venue: b[52],
            source: b[53],
            _pad: [b[54], b[55]],
            src_digest64: le_u64(b, 56),
        }
    }
}

/// The little-endian bytes of one row, field by field (the on-disk form).
pub(crate) fn row_bytes(b: &Bar) -> [u8; 64] {
    let mut out = [0u8; 64];
    let words = [
        b.open_1e6,
        b.high_1e6,
        b.low_1e6,
        b.close_1e6,
        b.volume_1e6,
        b.open_ms,
        b.close_ms,
    ];
    let mut i = 0;
    while i < 7 {
        out[8 * i..8 * i + 8].copy_from_slice(&words[i].to_le_bytes());
        i += 1;
    }
    out[56..60].copy_from_slice(&b.index.to_le_bytes());
    out[60..64].copy_from_slice(&b.flags.to_le_bytes());
    out
}

impl RowRec {
    /// Little-endian, field by field (the on-disk form; no padding exists to leak).
    // The frozen §3.6 signature borrows the 120-B record rather than copying it.
    #[allow(clippy::wrong_self_convention)]
    pub fn to_le_bytes(&self) -> [u8; 120] {
        let s = &self.summary;
        let mut b = [0u8; 120];
        b[..32].copy_from_slice(&self.trial_id);
        let u32s = [
            s.bars,
            s.trades,
            s.closed,
            s.obs_days,
            s.hold_bars_med,
            s.hold_bars_p95,
            s.insn_p9999,
            s.faults,
        ];
        let mut o = 32;
        let mut i = 0;
        while i < u32s.len() {
            b[o..o + 4].copy_from_slice(&u32s[i].to_le_bytes());
            o += 4;
            i += 1;
        }
        let i64s = [
            s.net_bps_1e4,
            s.gross_bps_1e4,
            s.fee_bps_1e4,
            s.max_concurrent_1e6,
            s.turnover_day_p99_1e6,
            s.sizing_notional_1e6,
        ];
        i = 0;
        while i < i64s.len() {
            b[o..o + 8].copy_from_slice(&i64s[i].to_le_bytes());
            o += 8;
            i += 1;
        }
        b[o..o + 4].copy_from_slice(&s.flags.to_le_bytes());
        b[o + 4..o + 8].copy_from_slice(&s._pad.to_le_bytes());
        b
    }

    /// Inverse of `to_le_bytes`.
    pub fn from_le_bytes(b: &[u8; 120]) -> RowRec {
        let u = |o: usize| le_u32(b, o);
        let q = |o: usize| le_u64(b, o) as i64;
        let mut trial_id = [0u8; 32];
        trial_id.copy_from_slice(&b[..32]);
        RowRec {
            trial_id,
            summary: TrialSummary {
                bars: u(32),
                trades: u(36),
                closed: u(40),
                obs_days: u(44),
                hold_bars_med: u(48),
                hold_bars_p95: u(52),
                insn_p9999: u(56),
                faults: u(60),
                net_bps_1e4: q(64),
                gross_bps_1e4: q(72),
                fee_bps_1e4: q(80),
                max_concurrent_1e6: q(88),
                turnover_day_p99_1e6: q(96),
                sizing_notional_1e6: q(104),
                flags: u(112),
                _pad: u(116),
            },
        }
    }
}

impl RowsHdr {
    /// Little-endian, field by field.
    // pin:07-rows-hdr-le: §3.6 fixes the layout, not the codec's name; `RowRec`'s shape.
    pub fn to_le_bytes(self) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[0..8].copy_from_slice(&self.magic);
        b[8..12].copy_from_slice(&self.version.to_le_bytes());
        b[12..16].copy_from_slice(&self.rec_size.to_le_bytes());
        b[16] = self.tier;
        b[17..24].copy_from_slice(&self._pad0);
        b[24..32].copy_from_slice(&self.sizing_notional_1e6.to_le_bytes());
        b[32..64].copy_from_slice(&self.fee_digest);
        b
    }

    /// Inverse of `to_le_bytes`.
    pub fn from_le_bytes(b: &[u8; 64]) -> RowsHdr {
        let mut fee = [0u8; 32];
        fee.copy_from_slice(&b[32..64]);
        RowsHdr {
            magic: [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]],
            version: le_u32(b, 8),
            rec_size: le_u32(b, 12),
            tier: b[16],
            _pad0: [b[17], b[18], b[19], b[20], b[21], b[22], b[23]],
            sizing_notional_1e6: le_u64(b, 24) as i64,
            fee_digest: fee,
        }
    }
}
