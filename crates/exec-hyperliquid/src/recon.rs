// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Reconciliation against the venue's own books (plan §6.2).
//!
//! Once a minute the worker asks the venue what it thinks we hold, and
//! compares that against what the engine believes. Drift beyond
//! `halt_on_recon_drift_usd_1e6` halts the lane.
//!
//! **This is the single most valuable safety net in the plan**, and
//! the reason is structural rather than clever: it is the only check
//! that is independent of every belief the engine holds. A lost fill,
//! a double-counted fill, a wrong asset id and a stale position view
//! all look perfectly consistent from inside the engine — each one is
//! a story the engine tells itself coherently. None of them survives
//! contact with the venue's balance sheet.
//!
//! ## HIP-4 netting
//!
//! Equal Yes and No holdings on one outcome are riskless collateral:
//! whichever way the outcome settles, one leg pays exactly what the
//! other costs. So exposure is `|yes − no|`, not `yes + no`.
//!
//! **The reconciler and the risk gate must compute it the same way or
//! they will disagree with the venue** — and a disagreement between
//! two of our own components, about a number the venue is the
//! authority on, is the worst possible place to discover a bug.
//!
//! ## Fail-closed, and why "empty" is not a safe default
//!
//! An unparseable body is an ERROR, never an empty balance set. The
//! temptation is real — a scanner that returns "no balances" on junk
//! never fails — but consider what it means when the engine also holds
//! nothing: the two agree, drift is zero, and the reconciler reports
//! everything fine having read nothing at all. The check would be
//! loudest exactly when it was working and silent exactly when it was
//! broken.
//!
//! The same reasoning makes an overflowing balance list an error
//! rather than a truncation: a position the caller never saw is a
//! position that reconciles by not being there.

use core_parse::{find_field, scan_price_1e8, skip_ws};

use crate::response::{ScanErr, Span};

/// One row of the venue's spot balance sheet.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct SpotBalance {
    /// The coin name, as a span into the scanned buffer. Left as a
    /// span rather than resolved here because HIP-4 outcome tokens are
    /// named by the venue and matching them is the caller's business,
    /// not the scanner's.
    pub coin: Span,
    /// Total holding, 1e8.
    pub total_1e8: i64,
    /// Amount held against open orders, 1e8.
    pub hold_1e8: i64,
}

impl SpotBalance {
    /// Total minus what is already committed to resting orders.
    #[inline]
    #[must_use]
    pub const fn free_1e8(&self) -> i64 {
        // Saturating, like everything else on this path: the scanner
        // can yield ±i64::MAX from a hostile body, and a panic here
        // would be in the reconciler — the one component whose job is
        // to keep working when the engine's own view is wrong.
        self.total_1e8.saturating_sub(self.hold_1e8)
    }
}

/// Scan a `spotClearinghouseState` body into `out`.
///
/// Returns how many balances were written.
///
/// # Errors
/// The body is not a shape this scanner recognises, or it holds more
/// balances than `out` can take. Both are refusals — see the module
/// docs for why neither may degrade to "no balances".
pub fn scan_spot_state(body: &[u8], out: &mut [SpotBalance]) -> Result<usize, ScanErr> {
    let pos = find_field(body, b"\"balances\"").ok_or(ScanErr::Malformed)?;
    // `find_field` lands after the key; step over `:` and any space to
    // the `[`.
    let mut i = skip_ws(body, pos);
    if i >= body.len() || body[i] != b':' {
        return Err(ScanErr::Malformed);
    }
    i = skip_ws(body, i + 1);
    if i >= body.len() || body[i] != b'[' {
        return Err(ScanErr::Malformed);
    }
    i += 1;

    let mut n = 0usize;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            // Ran off the end without a closing bracket: truncated.
            return Err(ScanErr::Malformed);
        }
        if body[i] == b']' {
            return Ok(n);
        }
        if body[i] == b',' {
            i += 1;
            continue;
        }
        if body[i] != b'{' {
            return Err(ScanErr::Malformed);
        }
        let obj_end = object_end(body, i).ok_or(ScanErr::Malformed)?;
        let obj = &body[i..obj_end];

        let coin = string_field(obj, b"\"coin\"").ok_or(ScanErr::Malformed)?;
        let total = decimal_field(obj, b"\"total\"").ok_or(ScanErr::Malformed)?;
        // `hold` is absent on some rows; absent means nothing is held,
        // which is a STATED default rather than a guess.
        let hold = decimal_field(obj, b"\"hold\"").unwrap_or(0);

        if n >= out.len() {
            return Err(ScanErr::Malformed);
        }
        out[n] = SpotBalance {
            // Spans are relative to the slice they were scanned in, so
            // shift the coin span back into `body`'s frame — the
            // caller resolves it against `body`, not against `obj`.
            coin: Span {
                start: coin.start + i as u32,
                end: coin.end + i as u32,
            },
            total_1e8: total,
            hold_1e8: hold,
        };
        n += 1;
        i = obj_end;
    }
}

/// Find the byte just past the object starting at `start`.
fn object_end(b: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = start;
    let mut in_str = false;
    let mut esc = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// `"key":"value"` → the span of `value`.
fn string_field(b: &[u8], key: &[u8]) -> Option<Span> {
    let p = find_field(b, key)?;
    let mut i = skip_ws(b, p);
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i = skip_ws(b, i + 1);
    if i >= b.len() || b[i] != b'"' {
        return None;
    }
    i += 1;
    let start = i;
    while i < b.len() && b[i] != b'"' {
        if b[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    Some(Span {
        start: start as u32,
        end: i as u32,
    })
}

/// `"key":"12.34"` → `1_234_000_000`. The venue quotes numbers as
/// STRINGS; a bare number is accepted too rather than refused.
fn decimal_field(b: &[u8], key: &[u8]) -> Option<i64> {
    let p = find_field(b, key)?;
    let mut i = skip_ws(b, p);
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i = skip_ws(b, i + 1);
    if i < b.len() && b[i] == b'"' {
        i += 1;
    }
    scan_price_1e8(b, i).map(|(v, _)| v)
}

/// HIP-4 exposure for one outcome: `|yes − no|`.
///
/// Equal legs are riskless collateral, so they net to nothing. See the
/// module docs — the risk gate must agree with this exactly.
#[inline(always)]
#[must_use]
pub fn net_exposure_1e8(yes_1e8: i64, no_1e8: i64) -> i64 {
    yes_1e8.saturating_sub(no_1e8).saturating_abs()
}

/// Drift between what the engine believes and what the venue says,
/// as an absolute magnitude in the same scale as the inputs.
#[inline(always)]
#[must_use]
pub fn drift(ours: i64, venue: i64) -> i64 {
    ours.saturating_sub(venue).saturating_abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"{"balances":[
        {"coin":"USDC","token":0,"total":"1234.56","hold":"12.00","entryNtl":"0.0"},
        {"coin":"+3253","token":107,"total":"10.00000001","hold":"0.0"},
        {"coin":"+3254","token":108,"total":"4"}
    ]}"#;

    fn coin<'a>(b: &'a [u8], s: &SpotBalance) -> &'a str {
        core::str::from_utf8(s.coin.of(b)).unwrap()
    }

    #[test]
    fn a_real_balance_sheet_scans() {
        let mut out = [SpotBalance::default(); 8];
        let n = scan_spot_state(BODY, &mut out).expect("scan");
        assert_eq!(n, 3);

        assert_eq!(coin(BODY, &out[0]), "USDC");
        assert_eq!(out[0].total_1e8, 123_456_000_000);
        assert_eq!(out[0].hold_1e8, 1_200_000_000);
        assert_eq!(out[0].free_1e8(), 122_256_000_000);

        // Full 1e8 resolution survives — this is the digit a 1e6
        // scanner would have thrown away.
        assert_eq!(coin(BODY, &out[1]), "+3253");
        assert_eq!(out[1].total_1e8, 1_000_000_001);

        // An absent `hold` is zero, stated rather than guessed.
        assert_eq!(coin(BODY, &out[2]), "+3254");
        assert_eq!(out[2].total_1e8, 400_000_000);
        assert_eq!(out[2].hold_1e8, 0);
    }

    /// **The property the whole module rests on.** Junk must never
    /// read as "no balances" — that reconciles a flat engine against a
    /// body nobody understood.
    #[test]
    fn an_unreadable_body_is_an_error_not_an_empty_sheet() {
        let mut out = [SpotBalance::default(); 8];
        for bad in [
            &b""[..],
            b"{}",
            b"not json at all",
            br#"{"balances":}"#,
            br#"{"balances":[{"coin":"USDC"}]}"#,          // no total
            br#"{"balances":[{"total":"1.0"}]}"#,          // no coin
            br#"{"balances":[{"coin":"USDC","total":"1"#,  // truncated
            br#"{"balances":[{"coin":"USDC","total":"1.0"},"#, // no close
        ] {
            assert!(
                scan_spot_state(bad, &mut out).is_err(),
                "read as empty: {:?}",
                String::from_utf8_lossy(bad)
            );
        }
        // An explicitly EMPTY sheet is legitimate and is not an error.
        assert_eq!(
            scan_spot_state(br#"{"balances":[]}"#, &mut out),
            Ok(0)
        );
    }

    /// A position the caller never saw is a position that reconciles
    /// by not being there.
    #[test]
    fn more_balances_than_the_caller_can_hold_is_an_error() {
        let mut out = [SpotBalance::default(); 2];
        assert!(scan_spot_state(BODY, &mut out).is_err());
        let mut out = [SpotBalance::default(); 3];
        assert_eq!(scan_spot_state(BODY, &mut out), Ok(3));
    }

    /// Equal legs are riskless collateral. The risk gate must agree
    /// with this function exactly.
    #[test]
    fn hip4_legs_net_rather_than_add() {
        assert_eq!(net_exposure_1e8(10, 10), 0, "equal legs are riskless");
        assert_eq!(net_exposure_1e8(10, 4), 6);
        assert_eq!(net_exposure_1e8(4, 10), 6, "direction does not matter");
        assert_eq!(net_exposure_1e8(0, 0), 0);
        // Hostile inputs saturate rather than panicking on the
        // reconcile path.
        assert_eq!(net_exposure_1e8(i64::MIN, i64::MAX), i64::MAX);
        assert_eq!(drift(i64::MIN, i64::MAX), i64::MAX);
    }

    #[test]
    fn drift_is_a_magnitude_in_either_direction() {
        assert_eq!(drift(100, 90), 10);
        assert_eq!(drift(90, 100), 10);
        assert_eq!(drift(0, 0), 0);
    }

    /// Spans must resolve against the buffer that was SCANNED — the
    /// same trap the response scanner's docs warn about, reached here
    /// because balances are parsed out of a sub-slice.
    #[test]
    fn coin_spans_resolve_against_the_whole_body() {
        let mut out = [SpotBalance::default(); 8];
        let n = scan_spot_state(BODY, &mut out).expect("scan");
        for b in &out[..n] {
            let s = b.coin.of(BODY);
            assert!(!s.is_empty(), "span resolved to nothing");
            assert!(
                s == b"USDC" || s.starts_with(b"+"),
                "span resolved to {:?}, which is not a coin name",
                String::from_utf8_lossy(s)
            );
        }
    }

    #[test]
    fn the_scanner_never_panics_on_arbitrary_bytes() {
        let mut out = [SpotBalance::default(); 4];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..20_000 {
            let mut buf = [0u8; 96];
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            let _ = scan_spot_state(&buf, &mut out);
        }
        // And truncations of a valid body, which is where a scanner
        // that trusts its own bounds actually breaks.
        for k in 0..BODY.len() {
            let _ = scan_spot_state(&BODY[..k], &mut out);
        }
    }
}
